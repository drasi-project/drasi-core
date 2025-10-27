use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Dispatch mode configuration for how changes are distributed to subscribers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DispatchMode {
    /// Broadcast mode: single channel with multiple receivers (1-to-N fanout)
    Broadcast,
    /// Channel mode: dedicated channel per subscriber (1-to-1)
    Channel,
}

impl Default for DispatchMode {
    fn default() -> Self {
        DispatchMode::Channel
    }
}

/// Trait for dispatching changes to subscribers
#[async_trait]
pub trait ChangeDispatcher<T>: Send + Sync
where
    T: Clone + Send + Sync + 'static,
{
    /// Dispatch a single change to all subscribers
    async fn dispatch_change(&self, change: Arc<T>) -> Result<()>;

    /// Dispatch multiple changes to all subscribers
    async fn dispatch_changes(&self, changes: Vec<Arc<T>>) -> Result<()> {
        for change in changes {
            self.dispatch_change(change).await?;
        }
        Ok(())
    }

    /// Create a new receiver for this dispatcher
    fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>>;
}

/// Trait for receiving changes from a dispatcher
#[async_trait]
pub trait ChangeReceiver<T>: Send + Sync
where
    T: Clone + Send + Sync + 'static,
{
    /// Receive the next change
    async fn recv(&mut self) -> Result<Arc<T>>;
}

/// Broadcast-based implementation of ChangeDispatcher
pub struct BroadcastChangeDispatcher<T>
where
    T: Clone + Send + Sync + 'static,
{
    tx: broadcast::Sender<Arc<T>>,
    _capacity: usize,
}

impl<T> BroadcastChangeDispatcher<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// Create a new broadcast dispatcher with specified capacity
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            _capacity: capacity,
        }
    }
}

#[async_trait]
impl<T> ChangeDispatcher<T> for BroadcastChangeDispatcher<T>
where
    T: Clone + Send + Sync + 'static,
{
    async fn dispatch_change(&self, change: Arc<T>) -> Result<()> {
        // Ignore send errors if there are no receivers
        let _ = self.tx.send(change);
        Ok(())
    }

    fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>> {
        let rx = self.tx.subscribe();
        Ok(Box::new(BroadcastChangeReceiver { rx }))
    }
}

/// Broadcast-based implementation of ChangeReceiver
pub struct BroadcastChangeReceiver<T>
where
    T: Clone + Send + Sync + 'static,
{
    rx: broadcast::Receiver<Arc<T>>,
}

#[async_trait]
impl<T> ChangeReceiver<T> for BroadcastChangeReceiver<T>
where
    T: Clone + Send + Sync + 'static,
{
    async fn recv(&mut self) -> Result<Arc<T>> {
        match self.rx.recv().await {
            Ok(change) => Ok(change),
            Err(broadcast::error::RecvError::Closed) => {
                Err(anyhow::anyhow!("Broadcast channel closed"))
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Log the lag but try to continue
                log::warn!("Broadcast receiver lagged by {} messages", n);
                // Try to receive the next message
                self.recv().await
            }
        }
    }
}

/// Channel-based (MPSC) implementation of ChangeDispatcher
pub struct ChannelChangeDispatcher<T>
where
    T: Clone + Send + Sync + 'static,
{
    tx: mpsc::Sender<Arc<T>>,
    rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<Arc<T>>>>>,
    _capacity: usize,
}

impl<T> ChannelChangeDispatcher<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// Create a new channel dispatcher with specified capacity
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self {
            tx,
            rx: Arc::new(tokio::sync::Mutex::new(Some(rx))),
            _capacity: capacity,
        }
    }
}

#[async_trait]
impl<T> ChangeDispatcher<T> for ChannelChangeDispatcher<T>
where
    T: Clone + Send + Sync + 'static,
{
    async fn dispatch_change(&self, change: Arc<T>) -> Result<()> {
        self.tx
            .send(change)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send on channel"))?;
        Ok(())
    }

    fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>> {
        // For channel mode, we can only create one receiver
        // Take the receiver out of the option
        let mut rx_opt = futures::executor::block_on(self.rx.lock());
        let rx = rx_opt
            .take()
            .ok_or_else(|| anyhow::anyhow!("Receiver already created for this channel dispatcher"))?;
        Ok(Box::new(ChannelChangeReceiver { rx }))
    }
}

/// Channel-based (MPSC) implementation of ChangeReceiver
pub struct ChannelChangeReceiver<T>
where
    T: Clone + Send + Sync + 'static,
{
    rx: mpsc::Receiver<Arc<T>>,
}

#[async_trait]
impl<T> ChangeReceiver<T> for ChannelChangeReceiver<T>
where
    T: Clone + Send + Sync + 'static,
{
    async fn recv(&mut self) -> Result<Arc<T>> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("Channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Duration};

    #[derive(Clone, Debug, PartialEq)]
    struct TestMessage {
        id: u32,
        content: String,
    }

    #[tokio::test]
    async fn test_broadcast_dispatcher_single_receiver() {
        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(100);
        let mut receiver = dispatcher.create_receiver().unwrap();

        let msg = Arc::new(TestMessage {
            id: 1,
            content: "test".to_string(),
        });

        dispatcher.dispatch_change(msg.clone()).await.unwrap();

        let received = receiver.recv().await.unwrap();
        assert_eq!(*received, *msg);
    }

    #[tokio::test]
    async fn test_broadcast_dispatcher_multiple_receivers() {
        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(100);
        let mut receiver1 = dispatcher.create_receiver().unwrap();
        let mut receiver2 = dispatcher.create_receiver().unwrap();

        let msg = Arc::new(TestMessage {
            id: 1,
            content: "broadcast".to_string(),
        });

        dispatcher.dispatch_change(msg.clone()).await.unwrap();

        let received1 = receiver1.recv().await.unwrap();
        let received2 = receiver2.recv().await.unwrap();

        assert_eq!(*received1, *msg);
        assert_eq!(*received2, *msg);
    }

    #[tokio::test]
    async fn test_broadcast_dispatcher_dispatch_changes() {
        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(100);
        let mut receiver = dispatcher.create_receiver().unwrap();

        let messages = vec![
            Arc::new(TestMessage {
                id: 1,
                content: "first".to_string(),
            }),
            Arc::new(TestMessage {
                id: 2,
                content: "second".to_string(),
            }),
            Arc::new(TestMessage {
                id: 3,
                content: "third".to_string(),
            }),
        ];

        dispatcher.dispatch_changes(messages.clone()).await.unwrap();

        for expected in messages {
            let received = receiver.recv().await.unwrap();
            assert_eq!(*received, *expected);
        }
    }

    #[tokio::test]
    async fn test_channel_dispatcher_single_receiver() {
        let dispatcher = ChannelChangeDispatcher::<TestMessage>::new(100);
        let mut receiver = dispatcher.create_receiver().unwrap();

        let msg = Arc::new(TestMessage {
            id: 1,
            content: "channel".to_string(),
        });

        dispatcher.dispatch_change(msg.clone()).await.unwrap();

        let received = receiver.recv().await.unwrap();
        assert_eq!(*received, *msg);
    }

    #[tokio::test]
    async fn test_channel_dispatcher_only_one_receiver() {
        let dispatcher = ChannelChangeDispatcher::<TestMessage>::new(100);
        let _receiver1 = dispatcher.create_receiver().unwrap();

        // Attempting to create a second receiver should fail
        let result = dispatcher.create_receiver();
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("Receiver already created"));
        }
    }

    #[tokio::test]
    async fn test_channel_dispatcher_dispatch_changes() {
        let dispatcher = ChannelChangeDispatcher::<TestMessage>::new(100);
        let mut receiver = dispatcher.create_receiver().unwrap();

        let messages = vec![
            Arc::new(TestMessage {
                id: 1,
                content: "first".to_string(),
            }),
            Arc::new(TestMessage {
                id: 2,
                content: "second".to_string(),
            }),
        ];

        dispatcher.dispatch_changes(messages.clone()).await.unwrap();

        for expected in messages {
            let received = receiver.recv().await.unwrap();
            assert_eq!(*received, *expected);
        }
    }

    #[tokio::test]
    async fn test_broadcast_receiver_handles_lag() {
        // Create a small capacity broadcaster to force lag
        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(2);
        let mut receiver = dispatcher.create_receiver().unwrap();

        // Send more messages than capacity without reading
        for i in 0..5 {
            let msg = Arc::new(TestMessage {
                id: i,
                content: format!("msg{}", i),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        // Give some time for messages to accumulate
        sleep(Duration::from_millis(10)).await;

        // Try to receive - should handle lag and continue
        let result = receiver.recv().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dispatch_mode_default() {
        assert_eq!(DispatchMode::default(), DispatchMode::Channel);
    }

    #[tokio::test]
    async fn test_dispatch_mode_serialization() {
        let mode = DispatchMode::Broadcast;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"broadcast\"");

        let deserialized: DispatchMode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DispatchMode::Broadcast);

        let mode = DispatchMode::Channel;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"channel\"");

        let deserialized: DispatchMode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DispatchMode::Channel);
    }
}