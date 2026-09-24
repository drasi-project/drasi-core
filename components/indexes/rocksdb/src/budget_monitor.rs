// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rocksdb::{Cache, WriteBufferManager};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const SATURATED_LOG_INTERVAL: Duration = Duration::from_secs(30);

// RocksDB starts shared write-buffer flush pressure when mutable memtable
// usage exceeds seven eighths of the configured budget. The Rust API exposes
// only total usage, which is always at least the mutable usage.
const SATURATION_NUMERATOR: usize = 7;
const SATURATION_DENOMINATOR: usize = 8;
// Recover one eighth below the saturation threshold to prevent log flapping.
const RECOVERY_NUMERATOR: usize = 3;
const RECOVERY_DENOMINATOR: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MemoryUsage {
    memtable_usage_bytes: usize,
    memtable_budget_bytes: usize,
    block_cache_usage_bytes: usize,
    block_cache_pinned_bytes: usize,
}

impl MemoryUsage {
    fn read(write_buffer_manager: &WriteBufferManager, block_cache: &Cache) -> Self {
        Self {
            memtable_usage_bytes: write_buffer_manager.get_usage(),
            memtable_budget_bytes: write_buffer_manager.get_buffer_size(),
            block_cache_usage_bytes: block_cache.get_usage(),
            block_cache_pinned_bytes: block_cache.get_pinned_usage(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaturationEventKind {
    Saturated,
    StillSaturated,
    Recovered,
}

impl SaturationEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Saturated => "rocksdb_memory_budget_saturated",
            Self::StillSaturated => "rocksdb_memory_budget_still_saturated",
            Self::Recovered => "rocksdb_memory_budget_recovered",
        }
    }

    fn log_level(self) -> log::Level {
        match self {
            Self::Saturated | Self::StillSaturated => log::Level::Warn,
            Self::Recovered => log::Level::Info,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SaturationEvent {
    kind: SaturationEventKind,
    usage: MemoryUsage,
}

impl fmt::Display for SaturationEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "event={} memtable_usage_bytes={} memtable_budget_bytes={} \
             block_cache_usage_bytes={} block_cache_pinned_bytes={}",
            self.kind.as_str(),
            self.usage.memtable_usage_bytes,
            self.usage.memtable_budget_bytes,
            self.usage.block_cache_usage_bytes,
            self.usage.block_cache_pinned_bytes,
        )
    }
}

enum SaturationState {
    Healthy,
    Saturated { last_logged_at: Instant },
}

struct SaturationTracker {
    state: SaturationState,
}

impl SaturationTracker {
    fn new() -> Self {
        Self {
            state: SaturationState::Healthy,
        }
    }

    fn observe(&mut self, now: Instant, usage: MemoryUsage) -> Option<SaturationEvent> {
        match self.state {
            SaturationState::Healthy => {
                if at_or_above_fraction(
                    usage.memtable_usage_bytes,
                    usage.memtable_budget_bytes,
                    SATURATION_NUMERATOR,
                    SATURATION_DENOMINATOR,
                ) {
                    self.state = SaturationState::Saturated {
                        last_logged_at: now,
                    };
                    Some(SaturationEvent {
                        kind: SaturationEventKind::Saturated,
                        usage,
                    })
                } else {
                    None
                }
            }
            SaturationState::Saturated { last_logged_at } => {
                if at_or_below_fraction(
                    usage.memtable_usage_bytes,
                    usage.memtable_budget_bytes,
                    RECOVERY_NUMERATOR,
                    RECOVERY_DENOMINATOR,
                ) {
                    self.state = SaturationState::Healthy;
                    Some(SaturationEvent {
                        kind: SaturationEventKind::Recovered,
                        usage,
                    })
                } else if now.saturating_duration_since(last_logged_at) >= SATURATED_LOG_INTERVAL {
                    self.state = SaturationState::Saturated {
                        last_logged_at: now,
                    };
                    Some(SaturationEvent {
                        kind: SaturationEventKind::StillSaturated,
                        usage,
                    })
                } else {
                    None
                }
            }
        }
    }
}

fn at_or_above_fraction(value: usize, total: usize, numerator: usize, denominator: usize) -> bool {
    total > 0 && value >= fraction_rounded_up(total, numerator, denominator)
}

fn at_or_below_fraction(value: usize, total: usize, numerator: usize, denominator: usize) -> bool {
    total > 0 && value <= fraction_rounded_down(total, numerator, denominator)
}

fn fraction_rounded_up(total: usize, numerator: usize, denominator: usize) -> usize {
    let whole = (total / denominator) * numerator;
    let remainder = (total % denominator) * numerator;
    whole + remainder.div_ceil(denominator)
}

fn fraction_rounded_down(total: usize, numerator: usize, denominator: usize) -> usize {
    (total / denominator) * numerator + ((total % denominator) * numerator) / denominator
}

fn log_event(event: SaturationEvent) {
    log::log!(event.kind.log_level(), "{event}");
}

/// Owns the worker that logs shared memory-budget pressure.
///
/// Dropping the monitor stops and joins the worker.
pub(crate) struct BudgetMonitor {
    shutdown: Option<Sender<()>>,
    // JoinHandle is not RefUnwindSafe. The worker state is never observed
    // after a panic, so wrapping it preserves the public types' unwind safety.
    worker: Option<AssertUnwindSafe<JoinHandle<()>>>,
}

impl BudgetMonitor {
    /// Starts a worker that samples usage every [`POLL_INTERVAL`].
    ///
    /// If thread creation fails, logs the error and returns an inactive monitor
    /// so diagnostics cannot prevent the index from starting.
    pub(crate) fn start(write_buffer_manager: WriteBufferManager, block_cache: Cache) -> Self {
        match spawn_worker(
            POLL_INTERVAL,
            move || MemoryUsage::read(&write_buffer_manager, &block_cache),
            log_event,
        ) {
            Ok(monitor) => monitor,
            Err(error) => {
                log::error!("event=rocksdb_memory_budget_monitor_start_failed error={error}");
                Self::inactive()
            }
        }
    }

    fn inactive() -> Self {
        Self {
            shutdown: None,
            worker: None,
        }
    }
}

impl Drop for BudgetMonitor {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(AssertUnwindSafe(worker)) = self.worker.take() {
            // Production never moves the monitor onto its worker, but avoid a
            // self-join deadlock if that ownership changes.
            if worker.thread().id() == thread::current().id() {
                return;
            }
            if worker.join().is_err() {
                log::error!("event=rocksdb_memory_budget_monitor_stopped reason=thread_panicked");
            }
        }
    }
}

fn spawn_worker<S, E>(
    poll_interval: Duration,
    mut sample: S,
    mut emit: E,
) -> std::io::Result<BudgetMonitor>
where
    S: FnMut() -> MemoryUsage + Send + 'static,
    E: FnMut(SaturationEvent) + Send + 'static,
{
    let (shutdown, shutdown_rx) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("rocksdb-budget-monitor".to_string())
        .spawn(move || {
            let mut tracker = SaturationTracker::new();
            loop {
                match shutdown_rx.recv_timeout(poll_interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(event) = tracker.observe(Instant::now(), sample()) {
                            emit(event);
                        }
                    }
                }
            }
        })?;

    Ok(BudgetMonitor {
        shutdown: Some(shutdown),
        worker: Some(AssertUnwindSafe(worker)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: usize = 800;

    fn usage(memtable_usage_bytes: usize) -> MemoryUsage {
        MemoryUsage {
            memtable_usage_bytes,
            memtable_budget_bytes: BUDGET,
            block_cache_usage_bytes: 1_200,
            block_cache_pinned_bytes: 300,
        }
    }

    #[test]
    fn enters_saturation_at_rocksdb_flush_pressure_threshold() {
        let now = Instant::now();
        let mut tracker = SaturationTracker::new();

        assert_eq!(tracker.observe(now, usage(699)), None);
        assert_eq!(
            tracker.observe(now, usage(700)),
            Some(SaturationEvent {
                kind: SaturationEventKind::Saturated,
                usage: usage(700),
            })
        );
    }

    #[test]
    fn hysteresis_prevents_flapping_and_reports_recovery() {
        let start = Instant::now();
        let mut tracker = SaturationTracker::new();

        assert!(tracker.observe(start, usage(700)).is_some());
        assert_eq!(
            tracker.observe(start + Duration::from_secs(1), usage(650)),
            None
        );
        assert_eq!(
            tracker.observe(start + Duration::from_secs(2), usage(600)),
            Some(SaturationEvent {
                kind: SaturationEventKind::Recovered,
                usage: usage(600),
            })
        );
        assert_eq!(
            tracker.observe(start + Duration::from_secs(3), usage(700)),
            Some(SaturationEvent {
                kind: SaturationEventKind::Saturated,
                usage: usage(700),
            })
        );
    }

    #[test]
    fn sustained_saturation_is_logged_at_the_periodic_cadence() {
        let start = Instant::now();
        let mut tracker = SaturationTracker::new();

        assert!(tracker.observe(start, usage(700)).is_some());
        assert_eq!(
            tracker.observe(
                start + SATURATED_LOG_INTERVAL - Duration::from_millis(1),
                usage(650)
            ),
            None
        );
        assert_eq!(
            tracker.observe(start + SATURATED_LOG_INTERVAL, usage(650)),
            Some(SaturationEvent {
                kind: SaturationEventKind::StillSaturated,
                usage: usage(650),
            })
        );
        assert_eq!(
            tracker.observe(
                start + SATURATED_LOG_INTERVAL * 2 - Duration::from_millis(1),
                usage(650)
            ),
            None
        );
        assert_eq!(
            tracker.observe(start + SATURATED_LOG_INTERVAL * 2, usage(650)),
            Some(SaturationEvent {
                kind: SaturationEventKind::StillSaturated,
                usage: usage(650),
            })
        );
    }

    #[test]
    fn every_event_formats_the_complete_usage_snapshot() {
        for kind in [
            SaturationEventKind::Saturated,
            SaturationEventKind::StillSaturated,
            SaturationEventKind::Recovered,
        ] {
            let event = SaturationEvent {
                kind,
                usage: usage(700),
            };
            assert_eq!(
                event.to_string(),
                format!(
                    "event={} memtable_usage_bytes=700 memtable_budget_bytes=800 \
                     block_cache_usage_bytes=1200 block_cache_pinned_bytes=300",
                    kind.as_str()
                )
            );
        }
    }

    #[test]
    fn saturation_warns_and_recovery_informs() {
        assert_eq!(SaturationEventKind::Saturated.log_level(), log::Level::Warn);
        assert_eq!(
            SaturationEventKind::StillSaturated.log_level(),
            log::Level::Warn
        );
        assert_eq!(SaturationEventKind::Recovered.log_level(), log::Level::Info);
    }

    #[test]
    fn dropping_monitor_stops_and_joins_worker() {
        let (event_tx, event_rx) = mpsc::channel();
        let (sample_tx, sample_rx) = mpsc::channel();
        let monitor = spawn_worker(
            Duration::from_millis(1),
            move || {
                sample_tx.send(()).expect("sample receiver");
                usage(700)
            },
            move |event| event_tx.send(event).expect("event receiver"),
        )
        .expect("spawn monitor");

        sample_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("monitor sampled");
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("saturation event"),
            SaturationEvent {
                kind: SaturationEventKind::Saturated,
                usage: usage(700),
            }
        );

        drop(monitor);
        sample_rx.try_iter().for_each(drop);
        assert_eq!(sample_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn fraction_thresholds_do_not_overflow() {
        assert_eq!(
            fraction_rounded_up(usize::MAX, SATURATION_NUMERATOR, SATURATION_DENOMINATOR),
            (usize::MAX / SATURATION_DENOMINATOR) * SATURATION_NUMERATOR
                + ((usize::MAX % SATURATION_DENOMINATOR) * SATURATION_NUMERATOR)
                    .div_ceil(SATURATION_DENOMINATOR)
        );
        assert_eq!(
            fraction_rounded_down(usize::MAX, RECOVERY_NUMERATOR, RECOVERY_DENOMINATOR),
            (usize::MAX / RECOVERY_DENOMINATOR) * RECOVERY_NUMERATOR
                + ((usize::MAX % RECOVERY_DENOMINATOR) * RECOVERY_NUMERATOR / RECOVERY_DENOMINATOR)
        );
    }
}
