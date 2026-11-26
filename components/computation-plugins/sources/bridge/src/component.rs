// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use async_trait::async_trait;
use drasi_computation_bridge_common::{
    receiver::{Observations, Receiver},
    transport::Listener,
    BridgeConfig, LinkStatus, Profile, TlsResource,
};
use drasi_computation_contracts::{ComponentDescriptor, ComponentRole, OutputEnvelope};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        DisposeContext, InitializeContext, LifecycleError, LifecycleResult, PauseContext,
        ResumeContext, StartContext, StopContext,
    },
    NativeAdmission, NativeComponent, NativeSource,
};
use futures_util::FutureExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

struct Runtime {
    receiver: Arc<Receiver>,
    queue: Option<mpsc::Receiver<OutputEnvelope>>,
    workers: Vec<JoinHandle<()>>,
    failures: watch::Receiver<Option<String>>,
}

/// The receiving endpoint. Construction binds nothing; Start binds the data
/// and control listeners and Stop joins their workers.
pub struct BridgeSource {
    config: BridgeConfig,
    graph_id: String,
    descriptor: ComponentDescriptor,
    admission: Option<NativeAdmission>,
    observations: Arc<Observations>,
    runtime: Option<Runtime>,
}

impl BridgeSource {
    pub fn new(
        config: BridgeConfig,
        graph_id: String,
        admission: Option<NativeAdmission>,
    ) -> anyhow::Result<Self> {
        config.validate_role(ComponentRole::Source)?;
        let descriptor = config.descriptor(ComponentRole::Source)?;
        drasi_computation_bridge_common::config::identifier(&graph_id)?;
        anyhow::ensure!(
            (config.profile == Profile::Durable) == admission.is_some(),
            "durable source requires admission; volatile source forbids it"
        );
        if let Some(admission) = &admission {
            let identity = admission.identity();
            // The journal's durability, subscribers and producer bound are
            // the Host's admission binding; only its scope is visible here.
            anyhow::ensure!(
                identity.component_id() == &config.receiver_endpoint
                    && identity.construction_scope() == config.receiver_instance
                    && identity.graph_id() == graph_id
                    && identity.stream() == &config.stream,
                "admission service scope, stream or component does not match bridge source"
            );
        }
        Ok(Self {
            config,
            graph_id,
            descriptor,
            admission,
            observations: Arc::new(Observations::default()),
            runtime: None,
        })
    }

    pub fn status(&self) -> LinkStatus {
        self.observations.snapshot()
    }

    fn tls(&self) -> anyhow::Result<Option<Arc<TlsResource>>> {
        let Some(recipe) = &self.config.tls else {
            return Ok(None);
        };
        let tls = recipe.resolve(Path::new("."))?;
        tls.validate_role(true)?;
        Ok(Some(tls))
    }

    /// Joins every worker within `budget`. Unjoined workers stay owned, so a
    /// retried Stop finishes the join.
    async fn join(&mut self, budget: Duration) -> LifecycleResult {
        let Some(runtime) = &mut self.runtime else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + budget;
        while let Some(worker) = runtime.workers.last_mut() {
            match tokio::time::timeout_at(deadline, worker).await {
                Ok(_) => {
                    runtime.workers.pop();
                }
                Err(_) => {
                    return Err(LifecycleError::retryable(anyhow::anyhow!(
                        "bridge workers did not finish before the stop deadline"
                    )))
                }
            }
        }
        self.runtime = None;
        Ok(())
    }
}

#[async_trait]
impl NativeComponent for BridgeSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        if let Some(recipe) = &self.config.tls {
            recipe.validate()?;
        }
        Ok(())
    }

    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if self.runtime.is_some() {
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "previous bridge workers must be stopped and joined before restart"
            )));
        }
        let tls = self.tls()?;
        let timeout = self.config.limits.timeout();
        let bind = async {
            let data = Listener::bind(&self.config.transport, false, tls.clone(), timeout).await?;
            let control =
                Listener::bind(&self.config.transport, true, tls.clone(), timeout).await?;
            drasi_computation_bridge_common::Result::Ok((data, control))
        };
        let (data, control) = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => return Err(LifecycleError::cancelled()),
            bound = bind => bound.map_err(anyhow::Error::from)?,
        };
        let (send, receive) = if self.config.profile == Profile::Volatile {
            let (send, receive) = mpsc::channel(self.config.limits.queue_capacity);
            (Some(send), Some(receive))
        } else {
            (None, None)
        };
        self.observations.stopping.store(false, Ordering::Release);
        self.observations.generation.store(0, Ordering::Release);
        self.observations
            .connected_generation
            .store(0, Ordering::Release);
        self.observations.in_flight.store(false, Ordering::Release);
        self.observations
            .unknown_sequence
            .store(0, Ordering::Release);
        self.observations.accepted.store(0, Ordering::Release);
        let receiver = Receiver::new(
            self.config.contract(true),
            &self.graph_id,
            self.admission.clone(),
            send,
            self.observations.clone(),
        )?;
        let (failures, failure_receiver) = watch::channel(None);
        let mut workers = Vec::with_capacity(2);
        for (index, listener) in [data, control].into_iter().enumerate() {
            let receiver = receiver.clone();
            let admission = self.admission.clone();
            let failures = failures.clone();
            workers.push(tokio::spawn(async move {
                let operation = async {
                    if index == 0 {
                        receiver.listen_data(listener).await
                    } else {
                        receiver.listen_control(listener).await
                    }
                };
                let failure = match std::panic::AssertUnwindSafe(operation).catch_unwind().await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(_) => Some("bridge I/O worker panicked".into()),
                };
                if let Some(failure) = failure {
                    if let Some(admission) = admission {
                        let _ = admission.report_failure(&failure);
                    }
                    failures.send_replace(Some(failure));
                }
            }));
        }
        self.runtime = Some(Runtime {
            receiver,
            queue: receive,
            workers,
            failures: failure_receiver,
        });
        Ok(())
    }

    /// Refuses new data with a retryable `Busy` and waits for any admission
    /// in flight. Receipt and control requests stay live, and frames already
    /// admitted stay in the queue or journal for delivery after Resume.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        let receiver = runtime.receiver.clone();
        let settled = async {
            match ctx.deadline() {
                Some(deadline) => tokio::time::timeout_at(deadline, receiver.pause())
                    .await
                    .is_ok(),
                None => {
                    receiver.pause().await;
                    true
                }
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => Err(LifecycleError::cancelled()),
            settled = settled => if settled {
                Ok(())
            } else {
                Err(LifecycleError::retryable(anyhow::anyhow!(
                    "bridge admission in flight did not finish before the pause deadline"
                )))
            },
        };
        if outcome.is_err() {
            runtime.receiver.resume();
        }
        outcome
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        if let Some(runtime) = &self.runtime {
            runtime.receiver.resume();
        }
        Ok(())
    }

    /// Cancels the listeners and joins their workers within the hook's
    /// deadline, bounded by the link timeout.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        self.observations.stopping.store(true, Ordering::Release);
        runtime.receiver.cancel.cancel();
        let mut budget = self.config.limits.timeout();
        if let Some(remaining) = ctx.scope().remaining() {
            budget = budget.min(remaining);
        }
        tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => Err(LifecycleError::cancelled()),
            joined = self.join(budget) => joined,
        }
    }

    /// The source owns no state: the admission journal belongs to the Host.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if ctx.erases_owned_state() {
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "this component does not support permanent state removal"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl NativeSource for BridgeSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let runtime = self
            .runtime
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("bridge source is not started"))?;
        let queue = runtime.queue.as_mut().ok_or_else(|| {
            anyhow::anyhow!("durable source must be driven through source admission")
        })?;
        loop {
            if let Some(failure) = runtime.failures.borrow().as_ref() {
                anyhow::bail!("bridge worker failed: {failure}");
            }
            tokio::select! {
                biased;
                _ = runtime.receiver.cancel.cancelled() => anyhow::bail!("bridge source stopped; EOF is not exhaustion"),
                changed = runtime.failures.changed() => { changed?; }
                envelope = queue.recv() => {
                    let envelope = envelope.ok_or_else(|| anyhow::anyhow!("bridge ingress closed unexpectedly"))?;
                    runtime.receiver.source_dequeued.store(envelope.envelope.system().sequence(), Ordering::Release);
                    return Ok(Some(envelope));
                },
            }
        }
    }
}

impl Drop for BridgeSource {
    fn drop(&mut self) {
        if let Some(runtime) = &self.runtime {
            runtime.receiver.cancel.cancel();
        }
    }
}
