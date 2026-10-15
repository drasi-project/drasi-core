// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    num::NonZeroUsize,
    sync::{atomic::Ordering, Arc},
};

use async_trait::async_trait;
use drasi_lib::{
    computation::v1::{
        AdmissionOptions, ComponentDescriptor, ComponentRecovery, ComponentRole,
        ComputationComponent, EnvelopeSource, OutputEnvelope, SourceAdmission,
    },
    context::workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
};
use futures_util::FutureExt;
use tokio::sync::{mpsc, watch, RwLock};
use tokio::task::JoinHandle;

use crate::{
    receiver::{Observations, Receiver},
    transport::Listener,
    BridgeConfig, BridgeIngressControl, LinkStatus, Profile, TlsResource,
};

struct Runtime {
    receiver: Arc<Receiver>,
    queue: Option<mpsc::Receiver<OutputEnvelope>>,
    workers: [RwLock<Option<JoinHandle<()>>>; 2],
    failures: watch::Receiver<Option<String>>,
}

pub struct BridgeSource {
    config: BridgeConfig,
    graph_id: String,
    descriptor: ComponentDescriptor,
    admission: Option<Arc<SourceAdmission>>,
    tls: Option<Arc<TlsResource>>,
    observations: Arc<Observations>,
    runtime: Option<Runtime>,
    ingress_control: Arc<BridgeIngressControl>,
}

impl BridgeSource {
    pub fn new(
        config: BridgeConfig,
        graph_id: String,
        admission: Option<Arc<SourceAdmission>>,
        tls: Option<Arc<TlsResource>>,
    ) -> anyhow::Result<Self> {
        let descriptor = config.descriptor(ComponentRole::Source)?;
        crate::config::identifier(&graph_id)?;
        anyhow::ensure!(
            (config.profile == Profile::Durable) == admission.is_some(),
            "durable source requires admission; volatile source forbids it"
        );
        anyhow::ensure!(
            config.transport.is_tls() == tls.is_some(),
            "TLS transport requires exactly its identity resource"
        );
        if let Some(tls) = &tls {
            tls.validate_role(true)?;
        }
        if let Some(admission) = &admission {
            anyhow::ensure!(
                admission.component_id().as_str() == config.receiver_endpoint
                    && admission.identity().construction_scope() == config.receiver_instance
                    && admission.identity().graph_id() == graph_id
                    && admission.port().as_str() == "out"
                    && admission.channel().definition().stream.as_str() == config.stream,
                "admission service scope, stream or component does not match bridge source"
            );
            admission
                .channel()
                .durability()
                .require(config.failure_scope)?;
            anyhow::ensure!(
                !admission.channel().definition().subscribers.is_empty()
                    && admission.channel().definition().subscribers.len() <= 16
                    && admission
                        .channel()
                        .definition()
                        .subscribers
                        .keys()
                        .all(|id| id.len() <= 256),
                "bridge journals require 1..=16 bounded subscribers"
            );
        }
        Ok(Self {
            config,
            graph_id,
            descriptor,
            admission,
            tls,
            observations: Arc::new(Observations::default()),
            runtime: None,
            ingress_control: Arc::new(BridgeIngressControl::new()),
        })
    }

    pub fn status(&self) -> LinkStatus {
        self.observations.snapshot()
    }

    pub fn ingress_control(&self) -> Arc<BridgeIngressControl> {
        self.ingress_control.clone()
    }

    pub fn with_ingress_control(mut self, control: Arc<BridgeIngressControl>) -> Self {
        self.ingress_control = control;
        self
    }
}

#[async_trait]
impl ComputationComponent for BridgeSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    fn recovery_contract(&self) -> ComponentRecovery {
        self.admission.as_ref().map_or_else(
            || ComponentRecovery::admitted(drasi_core::interface::StorageDurability::VOLATILE),
            |admission| ComponentRecovery::admitted(admission.channel().durability()),
        )
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.runtime.is_none(),
            "previous bridge workers must be stopped and joined before restart"
        );
        if let Some(admission) = &self.admission {
            // Idempotent verification of an already-enabled dedicated journal.
            // max_producers=1 pins the contract fingerprint in its producer name.
            admission
                .channel()
                .enable_admission(AdmissionOptions {
                    construction_scope: self.config.receiver_instance.clone(),
                    graph_id: self.graph_id.clone(),
                    component_id: self.descriptor.id().clone(),
                    failure_scope: self.config.failure_scope,
                    max_producers: NonZeroUsize::MIN,
                    receipts_per_producer: NonZeroUsize::MIN,
                })
                .await?;
        }
        let data = Listener::bind(
            &self.config.transport,
            false,
            self.tls.clone(),
            self.config.limits.timeout(),
        )
        .await?;
        let control = Listener::bind(
            &self.config.transport,
            true,
            self.tls.clone(),
            self.config.limits.timeout(),
        )
        .await?;
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
        let accepted = if let Some(admission) = &self.admission {
            admission.channel().progress().await?.accepted
        } else {
            0
        };
        self.observations
            .accepted
            .store(accepted, Ordering::Release);
        let receiver = Receiver::new(
            self.config.contract(true),
            &self.graph_id,
            self.admission.clone(),
            send,
            self.observations.clone(),
        )?;
        let (failures, failure_receiver) = watch::channel(None);
        self.runtime = Some(Runtime {
            receiver: receiver.clone(),
            queue: receive,
            workers: [RwLock::new(None), RwLock::new(None)],
            failures: failure_receiver,
        });
        let runtime = self.runtime.as_ref().expect("owned before spawning");
        self.ingress_control.bind(&runtime.receiver)?;
        for (index, listener) in [data, control].into_iter().enumerate() {
            let receiver = receiver.clone();
            let admission = self.admission.clone();
            let failures = failures.clone();
            spawn_owned_worker(&runtime.workers[index], async move {
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
            })
            .await?;
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        let Some(runtime) = &mut self.runtime else {
            return Ok(());
        };
        self.observations.stopping.store(true, Ordering::Release);
        runtime.receiver.cancel.cancel();
        for worker in &runtime.workers {
            let mut worker = worker.write().await;
            match join_owned_worker_gracefully(&mut worker, self.config.limits.timeout()).await? {
                WorkerCompletion::Absent | WorkerCompletion::Completed(()) => {}
                WorkerCompletion::Cancelled => {
                    anyhow::bail!("bridge worker was externally cancelled")
                }
            }
        }
        self.ingress_control.unbind(&runtime.receiver)?;
        self.runtime = None;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for BridgeSource {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        self.admission.clone()
    }

    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let runtime = self
            .runtime
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("bridge source is not started"))?;
        let queue = runtime.queue.as_mut().ok_or_else(|| {
            anyhow::anyhow!("durable source must be driven through SourceAdmission")
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
