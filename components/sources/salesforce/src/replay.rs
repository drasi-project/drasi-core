use std::collections::VecDeque;
use std::sync::{Arc, Weak};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_lib::channels::{ChangeReceiver, SourceEventWrapper};
use drasi_salesforce_common::LocalPosition;

use crate::store::{DurableMirrorStore, ReplayRetentionPin};

/// Composite receiver for per-subscriber durable catchup.
///
/// Delivers journal replay events from `(start, cut]` first, then forwards live
/// events strictly after `cut`, filtering duplicates that may have been
/// committed before the cut and dispatched after live registration.
pub struct JournalReplayReceiver {
    store: Arc<DurableMirrorStore>,
    next_ordinal: u64,
    cut_ordinal: u64,
    max_page_events: usize,
    max_page_bytes: usize,
    replay_buffer: VecDeque<Arc<SourceEventWrapper>>,
    live: Box<dyn ChangeReceiver<SourceEventWrapper>>,
    pin: Option<ReplayRetentionPin>,
    subscription_lifetime: Arc<()>,
}

impl JournalReplayReceiver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<DurableMirrorStore>,
        start_ordinal_exclusive: u64,
        cut_ordinal_inclusive: u64,
        max_page_events: usize,
        max_page_bytes: usize,
        pin: Option<ReplayRetentionPin>,
        live: Box<dyn ChangeReceiver<SourceEventWrapper>>,
    ) -> Self {
        Self {
            store,
            next_ordinal: start_ordinal_exclusive.saturating_add(1),
            cut_ordinal: cut_ordinal_inclusive,
            max_page_events,
            max_page_bytes,
            replay_buffer: VecDeque::new(),
            live,
            pin,
            subscription_lifetime: Arc::new(()),
        }
    }

    pub fn subscription_lifetime(&self) -> Weak<()> {
        Arc::downgrade(&self.subscription_lifetime)
    }
}

#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for JournalReplayReceiver {
    async fn recv(&mut self) -> Result<Arc<SourceEventWrapper>> {
        loop {
            if let Some(event) = self.replay_buffer.pop_front() {
                if self.replay_buffer.is_empty() && self.next_ordinal > self.cut_ordinal {
                    self.release_catchup_pin();
                }
                return Ok(event);
            }

            if self.next_ordinal <= self.cut_ordinal {
                let page = self
                    .store
                    .read_events_page(
                        self.next_ordinal,
                        self.cut_ordinal,
                        self.max_page_events,
                        self.max_page_bytes,
                    )
                    .await?;
                if page.is_empty() {
                    self.next_ordinal = self.cut_ordinal.saturating_add(1);
                    continue;
                }
                self.next_ordinal = page
                    .last()
                    .map(|event| required_event_ordinal(event.as_ref()))
                    .transpose()?
                    .map(|ordinal| ordinal.saturating_add(1))
                    .unwrap_or(self.next_ordinal);
                self.replay_buffer.extend(page);
                continue;
            }

            self.release_catchup_pin();
            let event = self.live.recv().await?;
            if let Some(ordinal) = event_ordinal_if_present(&event)? {
                if ordinal <= self.cut_ordinal {
                    continue;
                }
            }
            return Ok(event);
        }
    }
}

impl JournalReplayReceiver {
    fn release_catchup_pin(&mut self) {
        if self
            .pin
            .as_ref()
            .is_some_and(|pin| !pin.preserves_live_history())
        {
            self.pin = None;
        }
    }
}

fn required_event_ordinal(event: &SourceEventWrapper) -> Result<u64> {
    let source_position = event
        .source_position
        .as_ref()
        .context("replay page requires source_position on source events for ordinal progression")?;
    let position = LocalPosition::decode(source_position).map_err(|error| {
        anyhow!("failed to decode source_position for replay gating: {error:#}")
    })?;
    Ok(position.ordinal)
}

fn event_ordinal_if_present(event: &SourceEventWrapper) -> Result<Option<u64>> {
    let Some(source_position) = event.source_position.as_ref() else {
        return Ok(None);
    };
    let position = LocalPosition::decode(source_position).map_err(|error| {
        anyhow!("failed to decode source_position for replay gating: {error:#}")
    })?;
    Ok(Some(position.ordinal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{RecordMutation, StorageLimits};
    use anyhow::{anyhow, Result};
    use chrono::Utc;
    use drasi_core::models::ElementValue;
    use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
    use drasi_salesforce_common::{GraphRecord, LocalPosition, ObjectPlan, Properties};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    struct ChannelReceiver {
        rx: mpsc::UnboundedReceiver<Arc<SourceEventWrapper>>,
    }

    #[async_trait]
    impl ChangeReceiver<SourceEventWrapper> for ChannelReceiver {
        async fn recv(&mut self) -> Result<Arc<SourceEventWrapper>> {
            self.rx
                .recv()
                .await
                .ok_or_else(|| anyhow!("channel closed"))
        }
    }

    fn limits() -> StorageLimits {
        StorageLimits {
            max_rows: 100,
            max_events: 1000,
            retention_hours: 24,
            max_projected_row_bytes: 1024 * 1024,
            max_snapshot_leases: 4,
            max_spool_bytes: 8 * 1024 * 1024 * 1024,
        }
    }

    fn row(id: &str, name: &str, timestamp: u64) -> GraphRecord {
        let mut properties = Properties::new();
        properties.insert("Name".to_string(), ElementValue::String(name.into()));
        GraphRecord {
            object: "Account".into(),
            id: id.to_string(),
            properties,
            observed_at_ms: timestamp,
            relationships: Vec::new(),
            schema_revision: String::new(),
        }
    }

    fn insert(
        _source_id: &str,
        _org_id: &str,
        _plan: &ObjectPlan,
        row: GraphRecord,
    ) -> RecordMutation {
        RecordMutation::Upsert(row)
    }

    fn make_live_wrapper(
        source_id: &str,
        sequence: u64,
        ordinal: u64,
        label: &str,
    ) -> Arc<SourceEventWrapper> {
        let mut properties = drasi_core::models::ElementPropertyMap::new();
        properties.insert("Name", ElementValue::String(label.into()));
        let element = drasi_core::models::Element::Node {
            metadata: drasi_core::models::ElementMetadata {
                reference: drasi_core::models::ElementReference::new(
                    source_id,
                    &format!("sf:00D000000000001AAA:Account:{label}"),
                ),
                labels: vec![Arc::from("Account")].into(),
                effective_from: 0,
            },
            properties,
        };
        Arc::new(SourceEventWrapper {
            source_id: source_id.to_string(),
            event: SourceEvent::Change(drasi_core::models::SourceChange::Insert { element }),
            timestamp: Utc::now(),
            profiling: None,
            source_position: Some(LocalPosition::new(1, ordinal).encode()),
            sequence,
        })
    }

    #[tokio::test]
    async fn replay_receiver_pages_and_filters_cutover_duplicates() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let store = Arc::new(
            crate::store::DurableMirrorStore::open(
                temp.path().to_path_buf(),
                source_id.to_string(),
                "identity-v1".to_string(),
                org_id.to_string(),
                vec!["Account".to_string()],
                StorageLimits {
                    max_events: 3,
                    ..limits()
                },
            )
            .await
            .expect("open store"),
        );

        store
            .commit_mutations(vec![
                insert(source_id, org_id, &plan, row("001000000000001AAA", "A", 1)),
                insert(source_id, org_id, &plan, row("001000000000002AAA", "B", 2)),
                insert(source_id, org_id, &plan, row("001000000000003AAA", "C", 3)),
            ])
            .await
            .expect("commit");

        let (tx, rx) = mpsc::unbounded_channel();
        // Duplicate of the replay cut should be dropped.
        tx.send(make_live_wrapper(source_id, 3003, 3, "dup"))
            .expect("send duplicate");
        // First live event after cut should be delivered.
        tx.send(make_live_wrapper(source_id, 4004, 4, "live"))
            .expect("send live");
        let pin = store.register_replay_pin(1, 3).await.expect("replay pin");

        let mut receiver = JournalReplayReceiver::new(
            store.clone(),
            1,
            3,
            1,   // force paged replay
            256, // force byte-bounded replay pages
            pin,
            Box::new(ChannelReceiver { rx }),
        );

        let first = receiver.recv().await.expect("replay event 2");
        let second = receiver.recv().await.expect("replay event 3");
        store
            .commit_mutations(vec![
                insert(source_id, org_id, &plan, row("001000000000004AAA", "D", 4)),
                insert(source_id, org_id, &plan, row("001000000000005AAA", "E", 5)),
                insert(source_id, org_id, &plan, row("001000000000006AAA", "F", 6)),
            ])
            .await
            .expect("completed replay must no longer prevent retention");
        assert_eq!(
            store.earliest_resumable_ordinal().await.expect("earliest"),
            3
        );
        let third = receiver.recv().await.expect("live event 4");

        assert_eq!(first.sequence, 2);
        assert_eq!(second.sequence, 3);
        assert_eq!(third.sequence, 4004);
    }

    #[tokio::test]
    async fn replay_receiver_uses_local_position_ordinals_for_paging_and_live_cutover() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let store = Arc::new(
            crate::store::DurableMirrorStore::open(
                temp.path().to_path_buf(),
                source_id.to_string(),
                "identity-v1".to_string(),
                org_id.to_string(),
                vec!["Account".to_string()],
                limits(),
            )
            .await
            .expect("open store"),
        );

        // Ordinals 1..=4 are present in store.
        store
            .commit_mutations(vec![
                insert(source_id, org_id, &plan, row("001000000000001AAA", "A", 1)),
                insert(source_id, org_id, &plan, row("001000000000002AAA", "B", 2)),
                insert(source_id, org_id, &plan, row("001000000000003AAA", "C", 3)),
                insert(source_id, org_id, &plan, row("001000000000004AAA", "D", 4)),
            ])
            .await
            .expect("commit");

        let (tx, rx) = mpsc::unbounded_channel();
        // Duplicate by ordinal (4), but with high non-ordinal sequence.
        tx.send(make_live_wrapper(source_id, 104, 4, "dup"))
            .expect("send duplicate");
        // First true live event after cut has ordinal 5 and sequence 105.
        tx.send(make_live_wrapper(source_id, 105, 5, "live"))
            .expect("send live");
        let pin = store.register_replay_pin(1, 4).await.expect("replay pin");

        let mut receiver = JournalReplayReceiver::new(
            store,
            1,
            4,
            1,   // force multi-page replay
            256, // force byte-bounded replay pages
            pin,
            Box::new(ChannelReceiver { rx }),
        );

        let second = receiver.recv().await.expect("replay event ordinal 2");
        let third = receiver.recv().await.expect("replay event ordinal 3");
        let fourth = receiver.recv().await.expect("replay event ordinal 4");
        let live = receiver.recv().await.expect("live event ordinal 5");

        assert_eq!(required_event_ordinal(second.as_ref()).expect("ordinal"), 2);
        assert_eq!(required_event_ordinal(third.as_ref()).expect("ordinal"), 3);
        assert_eq!(required_event_ordinal(fourth.as_ref()).expect("ordinal"), 4);
        assert_eq!(required_event_ordinal(live.as_ref()).expect("ordinal"), 5);
        // Preserve envelope sequence (do not rewrite to ordinals).
        assert_eq!(live.sequence, 105);
    }
}
