// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{abi, transport::Failure, wire, Component, NativeTransactionContext};
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    BinaryEnvelopeCodec, ComputationComponent, DeliveryBatch, DeliveryBatchIdentity,
    DeliveryHandler, DeliveryItem, DeliveryScope, EnvelopeCodec, InputEnvelope, PortId, Schema,
};
use serde::{Deserialize, Serialize};
use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerMode {
    External,
    Transactional,
}

impl ConsumerMode {
    pub fn validate_factory(self, metadata: &crate::FactoryMetadata) -> anyhow::Result<()> {
        anyhow::ensure!(
            metadata.role == drasi_lib::computation::v1::ComponentRole::Sink
                && metadata.completion == Some(drasi_lib::computation::v1::SinkCompletion::Handled)
                && !metadata.capabilities.snapshot,
            "native consumer requires a handled, non-snapshot sink"
        );
        anyhow::ensure!(
            self != Self::Transactional
                || (!metadata.capabilities.control && !metadata.capabilities.readiness),
            "transactional consumers cannot expose independent control or readiness"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryConsumer {
    pub version: u32,
    pub mode: Option<ConsumerMode>,
}

pub trait NativeConsumer: ComputationComponent + DeliveryHandler {}
impl<T: ComputationComponent + DeliveryHandler> NativeConsumer for T {}

/// Only the borrowed host context may hold commit-sensitive business state.
/// No independent writes, external effects, workers or commit operation.
#[async_trait]
pub trait NativeTransactionalConsumer: ComputationComponent {
    fn retryable(&self, _error: &anyhow::Error) -> bool {
        false
    }
    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        context: &NativeTransactionContext<'_>,
    ) -> anyhow::Result<()>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginBatch {
    pub port: PortId,
    pub envelope: serde_bytes::ByteBuf,
    pub identity: DeliveryBatchIdentity,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleOperation {
    pub generation: u64,
    pub index: usize,
    pub id: String,
}

struct BatchSlot {
    generation: u64,
    batch: Option<Arc<DeliveryBatch>>,
}
pub(crate) struct ConsumerState {
    pub(crate) mode: ConsumerMode,
    scope: DeliveryScope,
    codec: EnvelopeCodec,
    batch: Mutex<BatchSlot>,
    interrupted: AtomicBool,
    damaged: AtomicBool,
}
struct CallGuard<'a> {
    state: &'a ConsumerState,
    completed: bool,
}
impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.state.interrupted.store(true, Ordering::Release);
        }
        if std::thread::panicking() {
            self.state.damaged.store(true, Ordering::Release);
        }
    }
}

impl ConsumerState {
    pub(crate) fn new(
        mode: ConsumerMode,
        request: &crate::CreateRequest,
        schemas: &[Arc<Schema>],
    ) -> anyhow::Result<Self> {
        let scope = request
            .scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native consumer requires graph scope"))?;
        let scope = DeliveryScope::new(
            scope.instance_id.clone(),
            scope.graph_id.clone(),
            request.id.clone(),
        )?;
        let mut codec =
            EnvelopeCodec::new(NonZeroUsize::new(abi::MAX_MESSAGE_BYTES).expect("wire bound"));
        for schema in schemas {
            codec.register_schema(schema.clone())?;
        }
        Ok(Self {
            mode,
            scope,
            codec,
            batch: Mutex::new(BatchSlot {
                generation: 0,
                batch: None,
            }),
            interrupted: AtomicBool::new(false),
            damaged: AtomicBool::new(false),
        })
    }
    pub(crate) fn check_ready(&self) -> Result<(), Failure> {
        if self.damaged.load(Ordering::Acquire) {
            return Err(Failure::failed(
                "native consumer panicked; reconstruct the component",
            ));
        }
        if self.interrupted.load(Ordering::Acquire) {
            return Err(Failure::failed(
                "native consumer requires completed cleanup",
            ));
        }
        Ok(())
    }
    pub(crate) async fn run(
        &self,
        work: impl std::future::Future<Output = Result<Vec<u8>, Failure>> + Send,
    ) -> Result<Vec<u8>, Failure> {
        let mut guard = CallGuard {
            state: self,
            completed: false,
        };
        tokio::pin!(work);
        let result = std::future::poll_fn(|context| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                work.as_mut().poll(context)
            })) {
                Ok(result) => result,
                Err(_) => {
                    self.damaged.store(true, Ordering::Release);
                    std::task::Poll::Ready(Err(Failure::panicked()))
                }
            }
        })
        .await;
        guard.completed = true;
        result
    }
    pub(crate) fn begin_batch(
        &self,
        input: &[u8],
        codec: &BinaryEnvelopeCodec,
        descriptor: &drasi_lib::computation::v1::ComponentDescriptor,
    ) -> anyhow::Result<Vec<u8>> {
        let input: BeginBatch = wire::decode(input)?;
        let envelope = codec.decode(&input.envelope)?;
        wire::check_port(
            descriptor,
            &input.port,
            &envelope,
            drasi_lib::computation::v1::PortDirection::Input,
        )?;
        let batch = DeliveryBatch::new(
            self.scope.clone(),
            InputEnvelope {
                port: input.port,
                envelope,
            },
            &self.codec,
        )?;
        anyhow::ensure!(
            batch.identity() == &input.identity,
            "native consumer batch identity/content mismatch"
        );
        let mut slot = self
            .batch
            .lock()
            .map_err(|_| anyhow::anyhow!("native consumer batch poisoned"))?;
        anyhow::ensure!(
            slot.batch.is_none(),
            "native consumer has an unreleased batch"
        );
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native consumer batch generation exhausted"))?;
        slot.batch = Some(Arc::new(batch));
        wire::encode(&slot.generation)
    }
    pub(crate) fn end_batch(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self
            .batch
            .lock()
            .map_err(|_| anyhow::anyhow!("native consumer batch poisoned"))?;
        anyhow::ensure!(
            generation != 0 && generation == slot.generation && slot.batch.is_some(),
            "native consumer batch is stale"
        );
        slot.batch = None;
        Ok(())
    }
    pub(crate) fn clear(&self) -> anyhow::Result<()> {
        self.batch
            .lock()
            .map_err(|_| anyhow::anyhow!("native consumer batch poisoned"))?
            .batch = None;
        self.interrupted.store(false, Ordering::Release);
        Ok(())
    }

    pub(crate) async fn handle(
        &self,
        component: &mut Component,
        input: &[u8],
        context: Option<&NativeTransactionContext<'_>>,
    ) -> Result<Vec<u8>, Failure> {
        let request: HandleOperation = wire::decode(input).map_err(Failure::from)?;
        let batch = {
            let slot = self
                .batch
                .lock()
                .map_err(|_| Failure::failed("native consumer batch poisoned"))?;
            if request.generation == 0 || request.generation != slot.generation {
                return Err(Failure::closed());
            }
            slot.batch.clone().ok_or_else(Failure::closed)?
        };
        let item = batch.item(request.index).map_err(Failure::from)?;
        if item.id.as_str() != request.id {
            return Err(Failure::protocol(
                "native consumer operation identity mismatch",
            ));
        }
        let (result, retryable) = match component {
            Component::Consumer(handler)
                if self.mode == ConsumerMode::External && context.is_none() =>
            {
                let result = handler.handle(item).await;
                let retryable = result
                    .as_ref()
                    .err()
                    .is_some_and(|error| handler.retryable(error));
                (result, retryable)
            }
            Component::TransactionalConsumer(handler)
                if self.mode == ConsumerMode::Transactional =>
            {
                let context = context.ok_or_else(|| {
                    Failure::protocol("native consumer requires the host transaction")
                })?;
                let result = handler.handle(item, context).await;
                let retryable = result
                    .as_ref()
                    .err()
                    .is_some_and(|error| handler.retryable(error));
                (result, retryable)
            }
            _ => return Err(Failure::protocol("native consumer handling mode mismatch")),
        };
        match result {
            Ok(()) => Ok(Vec::new()),
            Err(error) if retryable => {
                Err(Failure::new(abi::consumer::RETRYABLE, format!("{error:#}")))
            }
            Err(error) => Err(Failure::from(error)),
        }
    }
}
