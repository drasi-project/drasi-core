use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Event routing mode for distributing changes to subscribers
///
/// `DispatchMode` determines how events are routed from sources to queries and from
/// queries to reactions. It affects memory usage, throughput, and fanout behavior.
///
/// # Modes
///
/// ## Broadcast Mode
///
/// Uses a single shared channel with multiple receivers (1-to-N fanout):
///
/// ```text
/// Source → [Broadcast Channel] → Query 1
///                              → Query 2
///                              → Query 3
/// ```
///
/// **Advantages**:
/// - Lower memory usage (one copy of each event)
/// - Better for high-fanout scenarios (many subscribers)
/// - Automatic backpressure when slowest subscriber lags
///
/// **Disadvantages**:
/// - All subscribers receive all events (no filtering)
/// - Slowest subscriber can slow down entire system
/// - Events may be dropped if buffers fill
///
/// ## Channel Mode (Default)
///
/// Uses dedicated channels per subscriber (1-to-1):
///
/// ```text
/// Source → [Channel 1] → Query 1
///       → [Channel 2] → Query 2
///       → [Channel 3] → Query 3
/// ```
///
/// **Advantages**:
/// - Subscribers process independently
/// - Slow subscriber doesn't affect others
/// - More predictable behavior
///
/// **Disadvantages**:
/// - Higher memory usage (one copy per subscriber)
/// - More overhead for high-fanout scenarios
///
/// # Configuration
///
/// Set in YAML configuration or via builder API:
///
/// ```yaml
/// sources:
///   - id: data_source
///     source_type: postgres
///     dispatch_mode: broadcast  # or channel (default)
///
/// queries:
///   - id: my_query
///     query: "MATCH (n) RETURN n"
///     sources: [data_source]
///     dispatch_mode: channel
/// ```
///
/// # Choosing a Mode
///
/// **Use Broadcast when**:
/// - High fanout (10+ subscribers)
/// - All subscribers need all events
/// - Memory is constrained
/// - All subscribers process at similar speeds
///
/// **Use Channel (default) when**:
/// - Few subscribers (1-5)
/// - Subscribers have different processing speeds
/// - Isolation between subscribers is important
/// - Memory is not constrained
///
/// # Examples
///
/// ## Builder API Configuration
///
/// ```no_run
/// use drasi_lib::{DrasiLib, Query, DispatchMode};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Sources are now instance-based - create them externally and use .with_source()
/// let core = DrasiLib::builder()
///     // .with_source(my_source_instance)
///     .add_query(
///         Query::cypher("active_orders")
///             .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
///             .from_source("orders_db")
///             .with_dispatch_mode(DispatchMode::Channel)    // Default, independent processing
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## High-Fanout Scenario (Use Broadcast)
///
/// ```yaml
/// sources:
///   - id: event_stream
///     source_type: http
///     host: localhost
///     port: 8080
///     dispatch_mode: broadcast  # Many queries subscribe to this source
///
/// queries:
///   - id: query1
///     query: "MATCH (n:Type1) RETURN n"
///     sources: [event_stream]
///   - id: query2
///     query: "MATCH (n:Type2) RETURN n"
///     sources: [event_stream]
///   # ... 20 more queries subscribing to event_stream
/// ```
///
/// ## Independent Processing (Use Channel)
///
/// ```yaml
/// sources:
///   - id: sensor_data
///     source_type: mock
///     dispatch_mode: channel  # Default - each query processes independently
///
/// queries:
///   - id: real_time_alerts
///     query: "MATCH (s:Sensor) WHERE s.value > 100 RETURN s"
///     sources: [sensor_data]
///     # Fast processing
///
///   - id: historical_analysis
///     query: "MATCH (s:Sensor) RETURN s"
///     sources: [sensor_data]
///     # Slow processing - won't affect real_time_alerts
/// ```
///
/// # Performance Considerations
///
/// **Broadcast Memory Usage**: `O(buffer_size)` - single buffer shared
/// **Channel Memory Usage**: `O(buffer_size * subscribers)` - buffer per subscriber
///
/// For 10 subscribers with 1000-event buffers:
/// - Broadcast: ~1,000 events in memory
/// - Channel: ~10,000 events in memory
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
    async fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>>;
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

    async fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>> {
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

    async fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>> {
        // For channel mode, we can only create one receiver
        // Take the receiver out of the option
        let mut rx_opt = self.rx.lock().await;
        let rx = rx_opt.take().ok_or_else(|| {
            anyhow::anyhow!("Receiver already created for this channel dispatcher")
        })?;
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
        let mut receiver = dispatcher.create_receiver().await.unwrap();

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
        let mut receiver1 = dispatcher.create_receiver().await.unwrap();
        let mut receiver2 = dispatcher.create_receiver().await.unwrap();

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
        let mut receiver = dispatcher.create_receiver().await.unwrap();

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
        let mut receiver = dispatcher.create_receiver().await.unwrap();

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
        let _receiver1 = dispatcher.create_receiver().await.unwrap();

        // Attempting to create a second receiver should fail
        let result = dispatcher.create_receiver().await;
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("Receiver already created"));
        }
    }

    #[tokio::test]
    async fn test_channel_dispatcher_dispatch_changes() {
        let dispatcher = ChannelChangeDispatcher::<TestMessage>::new(100);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

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
        let mut receiver = dispatcher.create_receiver().await.unwrap();

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
