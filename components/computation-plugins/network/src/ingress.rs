// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::config::SourceConfig;
use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{NativeAdmission, Scope};
use drasi_core::models::SourceChange;
use drasi_lib::computation::v1::{
    ComponentDescriptor, GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress,
    OutputEnvelope, PortId,
};
use drasi_lib::context::workers::spawn_owned_worker;
use futures_util::FutureExt;
use std::{future::Future, net::SocketAddr, panic::AssertUnwindSafe, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{mpsc, Mutex, OwnedSemaphorePermit, RwLock, Semaphore},
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
    sender: Option<mpsc::Sender<OutputEnvelope>>,
    mode: AdmissionMode,
    requests: Arc<Semaphore>,
}
#[derive(Clone)]
enum AdmissionMode {
    Volatile {
        sequence: Arc<Mutex<u64>>,
        identity: GraphProducerIdentity,
    },
    Durable(NativeAdmission),
}
impl Ingress {
    pub fn durable(&self) -> Option<&NativeAdmission> {
        match &self.mode {
            AdmissionMode::Durable(service) => Some(service),
            AdmissionMode::Volatile { .. } => None,
        }
    }
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
            let AdmissionMode::Volatile { sequence, identity } = &self.mode else {
                anyhow::bail!("durable source requires the versioned admission protocol");
            };
            let sender = self
                .sender
                .as_ref()
                .context("volatile ingress is unavailable")?;
            let mut sequence = sequence.lock().await;
            for change in changes {
                let permit = sender.reserve().await.context("native ingress is closed")?;
                let next = sequence
                    .checked_add(1)
                    .context("native ingress sequence exhausted")?;
                let mut envelope = GraphChangeCodec::encode_change(
                    change,
                    self.config.stream.clone(),
                    next,
                    Some(chrono::Utc::now()),
                )?;
                GraphProducerProgress::annotate(&mut envelope, identity, next)?;
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
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    listener: RwLock<Option<JoinHandle<Result<()>>>>,
}
impl Running {
    async fn join(&mut self) -> Result<()> {
        if let Some(listener) = self.listener.get_mut() {
            let result = listener
                .await
                .context("native listener task failed")
                .and_then(|value| value);
            *self.listener.get_mut() = None;
            result?;
        }
        Ok(())
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(receiver) = &mut self.receiver {
            receiver.close();
        }
        if let Some(listener) = self.listener.get_mut() {
            listener.abort();
        }
    }
}

pub(crate) struct SourceRuntime {
    pub descriptor: ComponentDescriptor,
    pub config: SourceConfig,
    mode: AdmissionMode,
    running: Option<Running>,
}
impl SourceRuntime {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: SourceConfig,
        scope: Option<&Scope>,
        admission: Option<NativeAdmission>,
    ) -> Result<Self> {
        let mode = match admission {
            Some(service) => {
                anyhow::ensure!(
                    service.identity().stream() == &config.stream,
                    "native source stream does not match its admission channel"
                );
                AdmissionMode::Durable(service)
            }
            None => AdmissionMode::Volatile {
                sequence: Arc::new(Mutex::new(0)),
                identity: GraphProducerIdentity::volatile(
                    scope
                        .map_or("native-standalone", |scope| scope.instance_id.as_str())
                        .into(),
                    scope
                        .map_or("native-standalone", |scope| scope.graph_id.as_str())
                        .into(),
                    descriptor.id().clone(),
                    config.stream.clone(),
                )?,
            },
        };
        Ok(Self {
            descriptor,
            config,
            mode,
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
        let (sender, receiver) = match &self.mode {
            AdmissionMode::Volatile { .. } => {
                let (sender, receiver) = mpsc::channel(self.config.ingress_capacity);
                (Some(sender), Some(receiver))
            }
            AdmissionMode::Durable(_) => (None, None),
        };
        let cancel = CancellationToken::new();
        let ingress = Arc::new(Ingress {
            config: self.config.clone(),
            sender,
            cancel: cancel.clone(),
            mode: self.mode.clone(),
            requests: Arc::new(Semaphore::new(self.config.ingress_capacity)),
        });
        let stopped = cancel.clone();
        let admission = ingress.durable().cloned();
        let work = serve(listener, ingress);
        self.running = Some(Running {
            cancel,
            receiver,
            listener: RwLock::new(None),
        });
        spawn_owned_worker(
            &self.running.as_ref().expect("installed owner").listener,
            async move {
                let _cancel = stopped.clone().drop_guard();
                let result = AssertUnwindSafe(work)
                    .catch_unwind()
                    .await
                    .map_err(|panic| {
                        anyhow::anyhow!(
                            "native listener panicked: {}",
                            panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("non-string panic payload")
                        )
                    })
                    .and_then(|result| result);
                if let Some(admission) = admission {
                    if !stopped.is_cancelled() {
                        let mut reason = match &result {
                            Ok(()) => "native listener ended unexpectedly".to_owned(),
                            Err(error) => format!("{error:#}"),
                        };
                        eprintln!("drasi.network listener failed: {reason}");
                        if reason.len() > 4096 {
                            let mut end = 4093;
                            while !reason.is_char_boundary(end) {
                                end -= 1;
                            }
                            reason.truncate(end);
                            reason.push_str("...");
                        }
                        if let Err(error) = admission.report_failure(&reason) {
                            eprintln!(
                                "drasi.network listener failure could not reach graph: {error}"
                            );
                        }
                    }
                }
                result
            },
        )
        .await?;
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let running = self
            .running
            .as_mut()
            .context("native source has not started")?;
        let receiver = running
            .receiver
            .as_mut()
            .context("durable source is driven by graph admission, not next")?;
        tokio::select! {
            biased;
            _ = running.cancel.cancelled() => {
                running.join().await?;
                anyhow::bail!("native source listener closed");
            }
            envelope = receiver.recv() => match envelope {
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
            if let Some(receiver) = &mut running.receiver {
                receiver.close();
            }
            // Keep the handle on self until join completes: a cancelled stop
            // retains cleanup responsibility for the next stop invocation.
            running.join().await?;
            let mut discarded = 0;
            if let Some(receiver) = &mut running.receiver {
                while receiver.try_recv().is_ok() {
                    discarded += 1;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::computation::v1::ComponentId;
    use tokio::sync::Notify;

    fn runtime() -> Result<SourceRuntime> {
        SourceRuntime::new(
            ComponentDescriptor::try_new(ComponentId::try_new("test")?, vec![])?,
            SourceConfig::parse(
                serde_json::json!({"stream":"test","host":"127.0.0.1","port":0}),
                false,
            )?,
            None,
            None,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_or_panicked_listeners_require_cleanup_but_do_not_poison_retries() -> Result<()>
    {
        for panics in [false, true] {
            let mut runtime = runtime()?;
            runtime
                .start(move |_listener, _ingress| async move {
                    if panics {
                        panic!("listener panic probe");
                    }
                    anyhow::bail!("listener error probe");
                })
                .await?;
            runtime
                .running
                .as_ref()
                .expect("running owner")
                .cancel
                .cancelled()
                .await;
            let error = runtime.stop().await.expect_err("listener failure");
            assert!(
                error.to_string().contains(if panics {
                    "listener panic probe"
                } else {
                    "listener error probe"
                }),
                "{error:#}"
            );
            assert!(
                runtime.start(|_, _| async { Ok(()) }).await.is_err(),
                "failed cleanup still owns the run"
            );
            runtime.stop().await?;
            runtime
                .start(|_listener, ingress| async move {
                    ingress.cancel.cancelled().await;
                    Ok(())
                })
                .await?;
            runtime.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_stop_retains_listener_until_join_and_unexpected_exit_is_not_eof(
    ) -> Result<()> {
        let mut runtime = runtime()?;
        let entered = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let worker_entered = entered.clone();
        let worker_resume = resume.clone();
        runtime
            .start(move |_listener, ingress| async move {
                ingress.cancel.cancelled().await;
                worker_entered.notify_one();
                worker_resume.notified().await;
                Ok(())
            })
            .await?;
        tokio::select! {
            result = runtime.stop() => panic!("stop completed before listener exit: {result:?}"),
            _ = entered.notified() => {}
        }
        let listener = runtime
            .running
            .as_mut()
            .expect("cancelled stop retains owner")
            .listener
            .get_mut()
            .as_ref()
            .expect("cancelled stop retains join handle");
        assert!(!listener.is_finished());
        assert!(runtime.start(|_, _| async { Ok(()) }).await.is_err());
        resume.notify_one();
        runtime.stop().await?;
        runtime.start(|_, _| async { Ok(()) }).await?;
        assert!(
            runtime.next().await.is_err(),
            "listener exit must not be successful EOF"
        );
        runtime.stop().await?;
        Ok(())
    }
}
