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

//! Saturation monitor for the shared RocksDB memory budgets.
//!
//! The shared `WriteBufferManager` never blocks writers (`allow_stall` is
//! off): past the budget it applies continuous flush pressure instead, and
//! because memtable memory is charged to the shared block cache, sustained
//! saturation also evicts cached blocks and degrades every read. None of
//! that is visible in RocksDB's own LOG files — from the outside it looks
//! like the index simply got slow. This module polls the budget and logs
//! saturation transitions so a saturated budget is an observable state
//! rather than an inference from collapsed throughput.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rocksdb::{Cache, WriteBufferManager};

const MIB: usize = 1024 * 1024;

/// Poll interval for the saturation check.
const TICK_INTERVAL: Duration = Duration::from_secs(2);

/// While saturated, re-log the state every this many ticks (with 2s ticks,
/// every 30s) so long saturation windows stay visible without log spam.
const SATURATED_RELOG_TICKS: u32 = 15;

/// Handle owning the monitor thread. Held (via `Arc`) by every clone of the
/// `RocksDbTuning` it watches; dropping the last clone stops the thread on
/// its next tick.
pub(crate) struct BudgetMonitor {
    stop: Arc<AtomicBool>,
}

impl Drop for BudgetMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Spawn the saturation monitor for one shared budget.
pub(crate) fn spawn(wbm: WriteBufferManager, cache: Cache) -> BudgetMonitor {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    // Monitoring is best-effort: if the thread cannot be spawned the budget
    // still works, it is just unobserved again.
    let _ = std::thread::Builder::new()
        .name("rocksdb-budget-monitor".into())
        .spawn(move || {
            let mut saturated = false;
            let mut ticks_since_log = 0u32;
            loop {
                std::thread::sleep(TICK_INTERVAL);
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let usage = wbm.get_usage();
                let budget = wbm.get_buffer_size();
                let now_saturated = budget > 0 && usage >= budget;
                if now_saturated && !saturated {
                    log::warn!(
                        "rocksdb budget saturated: memtable usage {} MiB >= budget {} MiB — \
                         continuous flush pressure engaged; block cache usage {} MiB \
                         (pinned {} MiB). Writes are not stalled, but index throughput \
                         will degrade until flushes reclaim budget.",
                        usage / MIB,
                        budget / MIB,
                        cache.get_usage() / MIB,
                        cache.get_pinned_usage() / MIB,
                    );
                    ticks_since_log = 0;
                } else if now_saturated {
                    ticks_since_log += 1;
                    if ticks_since_log >= SATURATED_RELOG_TICKS {
                        log::warn!(
                            "rocksdb budget still saturated: memtable usage {} MiB / budget {} MiB; \
                             block cache usage {} MiB (pinned {} MiB)",
                            usage / MIB,
                            budget / MIB,
                            cache.get_usage() / MIB,
                            cache.get_pinned_usage() / MIB,
                        );
                        ticks_since_log = 0;
                    }
                } else if saturated {
                    log::info!(
                        "rocksdb budget recovered: memtable usage {} MiB back under budget {} MiB",
                        usage / MIB,
                        budget / MIB,
                    );
                }
                saturated = now_saturated;
            }
        });
    BudgetMonitor { stop }
}
