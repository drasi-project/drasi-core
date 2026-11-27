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

//! Streaming task: keeps an [`AhpSession`] open, diffs every mirror change
//! into `SourceChange`s, and resynchronises after connection loss.

use std::time::Duration;

use drasi_ahp_common::model::now_ms;
use drasi_ahp_common::{is_fatal, AhpClientConfig, AhpSession, ElementChange, GraphProjection};
use drasi_lib::channels::ComponentStatus;
use drasi_lib::sources::base::SourceBase;
use log::{debug, error, info, warn};
use tokio::sync::oneshot;

/// Timing settings for the streaming loop.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamTiming {
    pub keepalive: Option<Duration>,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
}

enum Wake {
    Shutdown,
    Event(anyhow::Result<Vec<drasi_ahp_common::Scope>>),
    Keepalive,
}

async fn dispatch(base: &SourceBase, source_id: &str, changes: Vec<ElementChange>) {
    if changes.is_empty() {
        return;
    }
    let effective_from = now_ms();
    debug!(
        "AHP source '{source_id}' dispatching {} changes",
        changes.len()
    );
    for change in changes {
        if let Err(e) = base
            .dispatch_source_change(change.to_source_change(source_id, effective_from))
            .await
        {
            warn!("AHP source '{source_id}' failed to dispatch change: {e}");
        }
    }
}

/// Sleep for `delay` unless shutdown is requested first. Returns `true` on shutdown.
async fn sleep_or_shutdown(delay: Duration, shutdown: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        biased;
        _ = shutdown => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

/// Run until shutdown or a fatal error.
///
/// The first successful snapshot only establishes the baseline when
/// `start()` is still waiting on `ready` (initial data then reaches queries
/// through the bootstrap provider, whose snapshot is taken later). If
/// `start()` stopped waiting, the first snapshot is emitted as inserts.
/// Every later snapshot, taken after a reconnect, is diffed against the last
/// emitted graph so changes missed while disconnected are emitted as
/// inserts/updates/deletes.
pub(crate) async fn run(
    base: SourceBase,
    source_id: String,
    config: AhpClientConfig,
    timing: StreamTiming,
    mut shutdown: oneshot::Receiver<()>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    let mut projection = GraphProjection::new();
    let mut ready = Some(ready);
    let mut backoff = timing.reconnect_initial;

    loop {
        let opened = tokio::select! {
            biased;
            _ = &mut shutdown => return,
            result = AhpSession::open(config.clone()) => result,
        };
        let mut session = match opened {
            Ok(session) => session,
            Err(e) if is_fatal(&e) => {
                error!("AHP source '{source_id}' cannot connect: {e:#}");
                base.set_status(ComponentStatus::Error, Some(format!("{e:#}")))
                    .await;
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(format!("{e:#}")));
                }
                return;
            }
            Err(e) => {
                warn!(
                    "AHP source '{source_id}' connection failed (retrying in {backoff:?}): {e:#}"
                );
                if sleep_or_shutdown(backoff, &mut shutdown).await {
                    return;
                }
                backoff = (backoff * 2).min(timing.reconnect_max);
                continue;
            }
        };
        backoff = timing.reconnect_initial;

        let changes = projection.refresh_all(session.mirror());
        let silent_baseline = ready.take().is_some_and(|ready| ready.send(Ok(())).is_ok());
        if silent_baseline {
            info!(
                "AHP source '{source_id}' synchronised {} graph elements",
                projection.len()
            );
        } else {
            info!(
                "AHP source '{source_id}' synchronised; emitting {} changes",
                changes.len()
            );
            dispatch(&base, &source_id, changes).await;
        }

        let mut keepalive = timing.keepalive.map(|period| {
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval
        });

        let failure = loop {
            let wake = tokio::select! {
                biased;
                _ = &mut shutdown => Wake::Shutdown,
                result = session.next_change() => Wake::Event(result),
                _ = async {
                    match keepalive.as_mut() {
                        Some(interval) => { interval.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => Wake::Keepalive,
            };
            match wake {
                Wake::Shutdown => {
                    session.close();
                    return;
                }
                Wake::Event(Ok(scopes)) => {
                    for scope in scopes {
                        let changes = projection.refresh(session.mirror(), &scope);
                        dispatch(&base, &source_id, changes).await;
                    }
                }
                Wake::Event(Err(e)) => break e,
                Wake::Keepalive => {
                    if let Err(e) = session.ping().await {
                        session.close();
                        break e;
                    }
                }
            }
        };

        warn!("AHP source '{source_id}' disconnected (reconnecting in {backoff:?}): {failure:#}");
        if sleep_or_shutdown(backoff, &mut shutdown).await {
            return;
        }
        backoff = (backoff * 2).min(timing.reconnect_max);
    }
}
