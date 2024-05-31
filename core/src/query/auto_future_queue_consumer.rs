use std::{
    sync::{atomic::AtomicU64, Arc},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::{
    evaluation::context::PhaseEvaluationContext,
    interface::{FutureElementRef, FutureQueueConsumer},
    models::SourceChange,
};

use super::ContinuousQuery;

pub struct AutoFutureQueueConsumer {
    continuous_query: Arc<ContinuousQuery>,
    channel_tx: mpsc::UnboundedSender<Vec<PhaseEvaluationContext>>,
    channel_rx: Mutex<mpsc::UnboundedReceiver<Vec<PhaseEvaluationContext>>>,
    now_override: Option<Arc<AtomicU64>>,
}

impl AutoFutureQueueConsumer {
    pub fn new(continuous_query: Arc<ContinuousQuery>) -> Self {
        let (channel_tx, channel_rx) = mpsc::unbounded_channel();

        AutoFutureQueueConsumer {
            continuous_query,
            channel_tx,
            channel_rx: Mutex::new(channel_rx),
            now_override: None,
        }
    }

    pub fn with_now_override(mut self, now_override: Arc<AtomicU64>) -> Self {
        self.now_override = Some(now_override);
        self
    }

    pub async fn recv(&self, timeout: Duration) -> Option<Vec<PhaseEvaluationContext>> {
        let mut rx = self.channel_rx.lock().await;
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(result)) => Some(result),
            Ok(None) => None,
            Err(_) => None,
        }
    }
}

#[async_trait]
impl FutureQueueConsumer for AutoFutureQueueConsumer {
    async fn on_due(
        &self,
        future_ref: &FutureElementRef,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let change = SourceChange::Future {
            future_ref: future_ref.clone(),
        };

        let result = self.continuous_query.process_source_change(change).await?;
        if !result.is_empty() {
            self.channel_tx.send(result)?;
        }
        Ok(())
    }
    async fn on_error(
        &self,
        future_ref: &FutureElementRef,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) {
        log::error!(
            "Error processing {} off future queue: {:?}",
            future_ref.element_ref,
            error
        );
    }

    fn now(&self) -> u64 {
        if let Some(now_override) = &self.now_override {
            return now_override.load(std::sync::atomic::Ordering::Relaxed);
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            * 1000;
        now
    }
}
