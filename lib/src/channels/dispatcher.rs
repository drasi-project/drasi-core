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
///     .with_query(
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DispatchMode {
    /// Broadcast mode: single channel with multiple receivers (1-to-N fanout)
    Broadcast,
    /// Channel mode: dedicated channel per subscriber (1-to-1)
    #[default]
    Channel,
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
    /// Receive the next change from the broadcast channel.
    ///
    /// # Lag Handling
    ///
    /// When the receiver falls behind the sender (lag), this method uses an
    /// **iterative loop** instead of recursion to handle repeated lag events.
    /// This prevents stack overflow under sustained high throughput where a slow
    /// subscriber might experience continuous lag.
    ///
    /// # Returns
    ///
    /// * `Ok(Arc<T>)` - The next available message
    /// * `Err(...)` - If the broadcast channel is closed
    async fn recv(&mut self) -> Result<Arc<T>> {
        loop {
            match self.rx.recv().await {
                Ok(change) => return Ok(change),
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow::anyhow!("Broadcast channel closed"));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Log the lag and continue with the loop (non-recursive)
                    // This prevents stack overflow under sustained lag conditions
                    log::warn!("Broadcast receiver lagged by {n} messages, continuing...");
                    // Loop continues to try receiving the next available message
                    continue;
                }
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
                content: format!("msg{i}"),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        // Give some time for messages to accumulate
        sleep(Duration::from_millis(10)).await;

        // Try to receive - should handle lag and continue
        let result = receiver.recv().await;
        assert!(result.is_ok());
    }

    // =============================================================================
    // TESTS FOR BUG FIX: Unbounded Recursion in BroadcastChangeReceiver
    // =============================================================================
    //
    // These tests verify that the BroadcastChangeReceiver handles lag events
    // using an iterative loop instead of recursion, preventing stack overflow.

    #[tokio::test]
    async fn test_broadcast_receiver_handles_multiple_consecutive_lags_without_stack_overflow() {
        // This test would cause stack overflow with the old recursive implementation
        // when many lag events occur consecutively.
        //
        // The fix uses an iterative loop, so this test should pass without issues.

        // Use a very small buffer (capacity 1) to maximize lag probability
        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(1);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        // Send many more messages than buffer capacity
        // With capacity=1, almost every message after the first will cause lag
        let num_messages = 100;
        for i in 0..num_messages {
            let msg = Arc::new(TestMessage {
                id: i,
                content: format!("msg{i}"),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        // Give time for messages to be sent
        sleep(Duration::from_millis(50)).await;

        // Now try to receive - this would have caused stack overflow before the fix
        // because each lag triggers another recv() call recursively.
        // With the iterative fix, it simply loops until it gets a message.
        let result = tokio::time::timeout(Duration::from_secs(1), receiver.recv()).await;

        assert!(
            result.is_ok(),
            "recv() should complete without timeout or stack overflow"
        );
        assert!(
            result.unwrap().is_ok(),
            "recv() should return a message after handling lag"
        );
    }

    #[tokio::test]
    async fn test_broadcast_receiver_survives_sustained_high_throughput_with_slow_consumer() {
        // This test simulates a production scenario where:
        // 1. A fast producer sends messages continuously
        // 2. A slow consumer can't keep up
        // 3. The consumer experiences repeated lag events
        //
        // Before the fix: Would eventually stack overflow
        // After the fix: Handles lag gracefully and continues processing

        let dispatcher = Arc::new(BroadcastChangeDispatcher::<TestMessage>::new(5));
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        // Counter to track received messages
        let received_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let received_count_clone = received_count.clone();

        // Spawn producer that sends messages faster than consumer can process
        let dispatcher_clone = dispatcher.clone();
        let producer = tokio::spawn(async move {
            for i in 0..500 {
                let msg = Arc::new(TestMessage {
                    id: i,
                    content: format!("high_throughput_{i}"),
                });
                let _ = dispatcher_clone.dispatch_change(msg).await;
                // No delay - send as fast as possible
            }
        });

        // Consumer with artificial slowdown
        let consumer = tokio::spawn(async move {
            for _ in 0..10 {
                // Only try to receive 10 messages
                match tokio::time::timeout(Duration::from_millis(500), receiver.recv()).await {
                    Ok(Ok(_msg)) => {
                        received_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // Simulate slow processing
                        sleep(Duration::from_millis(10)).await;
                    }
                    Ok(Err(_)) => break, // Channel closed
                    Err(_) => break,     // Timeout
                }
            }
        });

        // Wait for both to complete
        let _ = producer.await;
        let consumer_result = tokio::time::timeout(Duration::from_secs(5), consumer).await;

        assert!(
            consumer_result.is_ok(),
            "Consumer should complete without hanging or stack overflow"
        );

        let count = received_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            count > 0,
            "Consumer should have received at least some messages despite lag, got {count}"
        );
    }

    #[tokio::test]
    async fn test_broadcast_receiver_handles_extreme_lag_scenario() {
        // Extreme test: Force hundreds of lag events in rapid succession
        // This would definitely cause stack overflow with recursive implementation

        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(1);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        // Send 1000 messages with capacity=1, causing ~999 lag events
        for i in 0..1000 {
            let msg = Arc::new(TestMessage {
                id: i,
                content: format!("extreme_{i}"),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        // The receiver is now extremely lagged
        // With recursive implementation, trying to recv() would cause:
        // recv() -> Lagged -> recv() -> Lagged -> recv() -> ... -> STACK OVERFLOW
        //
        // With iterative implementation:
        // loop { recv() -> Lagged -> continue -> recv() -> Lagged -> continue -> ... -> Ok(msg) }

        let result = tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await;

        assert!(
            result.is_ok(),
            "Should not timeout - iterative lag handling should complete quickly"
        );
        assert!(
            result.unwrap().is_ok(),
            "Should successfully receive a message after handling extreme lag"
        );
    }

    #[tokio::test]
    async fn test_broadcast_receiver_recovers_and_continues_after_lag() {
        // Test that after experiencing lag, the receiver can continue
        // receiving messages normally

        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(2);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        // Phase 1: Cause lag
        for i in 0..10 {
            let msg = Arc::new(TestMessage {
                id: i,
                content: format!("phase1_{i}"),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        sleep(Duration::from_millis(10)).await;

        // Receive one message (handles lag internally)
        let result1 = receiver.recv().await;
        assert!(result1.is_ok(), "Should handle lag and receive message");

        // Phase 2: Send more messages after receiver has caught up
        let msg_after_lag = Arc::new(TestMessage {
            id: 100,
            content: "after_lag".to_string(),
        });
        dispatcher.dispatch_change(msg_after_lag.clone()).await.unwrap();

        // Should receive this message normally
        let result2 = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
        assert!(result2.is_ok(), "Should receive message without timeout");
        let received = result2.unwrap().unwrap();
        assert_eq!(received.id, 100, "Should receive the correct message");
        assert_eq!(received.content, "after_lag");
    }

    #[tokio::test]
    async fn test_broadcast_receiver_iterative_lag_handling_performance() {
        // Verify that iterative lag handling has consistent performance
        // regardless of the number of lag events

        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(1);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        // Send enough messages to cause significant lag
        for i in 0..500 {
            let msg = Arc::new(TestMessage {
                id: i,
                content: format!("perf_{i}"),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        // Time how long it takes to receive a message
        let start = std::time::Instant::now();
        let result = receiver.recv().await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        // With iterative handling, this should be fast (milliseconds, not seconds)
        // Recursive handling would either stack overflow or be very slow due to
        // deep call stack overhead
        assert!(
            elapsed < Duration::from_secs(1),
            "Lag handling should be fast, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_broadcast_receiver_lag_does_not_drop_all_messages() {
        // Verify that after lag, we can still receive *some* messages
        // (the most recent ones in the buffer)

        let dispatcher = BroadcastChangeDispatcher::<TestMessage>::new(3);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        // Send messages that will overflow the buffer
        for i in 0..20 {
            let msg = Arc::new(TestMessage {
                id: i,
                content: format!("msg_{i}"),
            });
            dispatcher.dispatch_change(msg).await.unwrap();
        }

        sleep(Duration::from_millis(10)).await;

        // We should be able to receive the messages that are still in the buffer
        let mut received_ids = Vec::new();
        for _ in 0..3 {
            match tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await {
                Ok(Ok(msg)) => received_ids.push(msg.id),
                _ => break,
            }
        }

        assert!(
            !received_ids.is_empty(),
            "Should have received at least one message"
        );
        // The received messages should be among the later ones (not the first ones that were dropped)
        assert!(
            received_ids.iter().all(|&id| id >= 10),
            "Received messages should be from later sends (after lag dropped early ones): {received_ids:?}"
        );
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
