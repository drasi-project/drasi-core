// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::config::SourceConfig;
use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::Scope;
use drasi_core::models::SourceChange;
use drasi_lib::computation::v1::{
    ComponentDescriptor, GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress,
    OutputEnvelope, PortId,
};
use std::{future::Future, net::SocketAddr, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub(crate) struct AdmissionError {
    pub accepted: usize,
    pub error: anyhow::Error,
}
pub(crate) struct Ingress {
    pub config: SourceConfig,
    pub cancel: CancellationToken,
    sender: mpsc::Sender<OutputEnvelope>,
    sequence: Arc<Mutex<u64>>,
    identity: GraphProducerIdentity,
    requests: Arc<Semaphore>,
}
impl Ingress {
    pub fn request_permit(&self) -> Result<OwnedSemaphorePermit> {
        anyhow::ensure!(!self.cancel.is_cancelled(), "native ingress is stopped");
        self.requests
            .clone()
            .try_acquire_owned()
            .context("native ingress concurrent-request limit reached")
    }
    pub async fn admit(
        &self,
        changes: Vec<SourceChange>,
    ) -> std::result::Result<usize, AdmissionError> {
        let mut accepted = 0;
        let submit = async {
            let mut sequence = self.sequence.lock().await;
            for change in changes {
                let permit = self
                    .sender
                    .reserve()
                    .await
                    .context("native ingress is closed")?;
                let next = sequence
                    .checked_add(1)
                    .context("native ingress sequence exhausted")?;
                let mut envelope = GraphChangeCodec::encode_change(
                    change,
                    self.config.stream.clone(),
                    next,
                    Some(chrono::Utc::now()),
                )?;
                GraphProducerProgress::annotate(&mut envelope, &self.identity, next)?;
                // No await between sequence assignment and enqueue. Cancellation
                // while waiting for capacity cannot consume an event sequence.
                *sequence = next;
                permit.send(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope,
                });
                accepted += 1;
            }
            Result::<()>::Ok(())
        };
        let deadline = Instant::now() + std::time::Duration::from_millis(self.config.timeout_ms);
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(anyhow::anyhow!("native ingress was stopped")),
            result = tokio::time::timeout_at(deadline, submit) =>
                result.map_err(|_| anyhow::anyhow!("native ingress capacity/admission timeout")).and_then(|result| result),
        };
        match result {
            Ok(()) => Ok(accepted),
            Err(error) => Err(AdmissionError { accepted, error }),
        }
    }
}

struct Running {
    cancel: CancellationToken,
    receiver: mpsc::Receiver<OutputEnvelope>,
    listener: Option<JoinHandle<Result<()>>>,
    failure: Option<String>,
}
impl Running {
    async fn join(&mut self) -> Result<()> {
        if let Some(listener) = &mut self.listener {
            let result = listener
                .await
                .context("native listener task failed")
                .and_then(|value| value);
            self.listener = None;
            if let Err(error) = result {
                self.failure = Some(format!("{error:#}"));
            }
        }
        match &self.failure {
            Some(error) => Err(anyhow::anyhow!("{error}")),
            None => Ok(()),
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.receiver.close();
        if let Some(listener) = &self.listener {
            listener.abort();
        }
    }
}

pub(crate) struct SourceRuntime {
    pub descriptor: ComponentDescriptor,
    pub config: SourceConfig,
    identity: GraphProducerIdentity,
    sequence: Arc<Mutex<u64>>,
    running: Option<Running>,
}
impl SourceRuntime {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: SourceConfig,
        scope: Option<&Scope>,
    ) -> Result<Self> {
        let identity = GraphProducerIdentity::volatile(
            scope
                .map_or("native-standalone", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .map_or("native-standalone", |scope| scope.graph_id.as_str())
                .into(),
            descriptor.id().clone(),
            config.stream.clone(),
        )?;
        Ok(Self {
            descriptor,
            config,
            identity,
            sequence: Arc::new(Mutex::new(0)),
            running: None,
        })
    }
    pub async fn start<F, Work>(&mut self, serve: F) -> Result<()>
    where
        F: FnOnce(TcpListener, Arc<Ingress>) -> Work + Send,
        Work: Future<Output = Result<()>> + Send + 'static,
    {
        anyhow::ensure!(
            self.running.is_none(),
            "native source is already started or still needs cleanup"
        );
        let address = SocketAddr::new(self.config.host.parse()?, self.config.port);
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("bind native source to {address}"))?;
        eprintln!(
            "drasi.network listener started: component={} address={}",
            self.descriptor.id(),
            listener.local_addr()?
        );
        let (sender, receiver) = mpsc::channel(self.config.ingress_capacity);
        let cancel = CancellationToken::new();
        let ingress = Arc::new(Ingress {
            config: self.config.clone(),
            sender,
            cancel: cancel.clone(),
            sequence: self.sequence.clone(),
            identity: self.identity.clone(),
            requests: Arc::new(Semaphore::new(self.config.ingress_capacity)),
        });
        let stopped = cancel.clone();
        let work = serve(listener, ingress);
        // The guard also cancels ingress if the listener panics.
        let listener = tokio::spawn(async move {
            let _cancel = stopped.drop_guard();
            work.await
        });
        self.running = Some(Running {
            cancel,
            receiver,
            listener: Some(listener),
            failure: None,
        });
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let running = self
            .running
            .as_mut()
            .context("native source has not started")?;
        tokio::select! {
            biased;
            _ = running.cancel.cancelled() => {
                running.join().await?;
                anyhow::bail!("native source listener closed");
            }
            envelope = running.receiver.recv() => match envelope {
                Some(envelope) => Ok(Some(envelope)),
                None => {
                    running.join().await?;
                    anyhow::bail!("native ingress closed unexpectedly");
                }
            },
        }
    }
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(running) = &mut self.running {
            running.cancel.cancel();
            running.receiver.close();
            // Keep the handle on self until join completes: a cancelled stop
            // retains cleanup responsibility for the next stop invocation.
            running.join().await?;
            let mut discarded = 0;
            while running.receiver.try_recv().is_ok() {
                discarded += 1;
            }
            if discarded > 0 {
                eprintln!("drasi.network volatile ingress stopped: component={} discarded_unprocessed_events={discarded}",
                    self.descriptor.id());
            }
        }
        self.running = None;
        Ok(())
    }
    pub fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
}
