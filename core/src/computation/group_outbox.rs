// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use async_trait::async_trait;

use crate::interface::{IndexError, OutboxPageLimits, OutboxWriter};

pub(super) struct GroupOutbox {
    inner: Arc<dyn OutboxWriter>,
    prefix: String,
}

fn append_hex(target: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in value.bytes() {
        target.push(char::from(HEX[usize::from(byte >> 4)]));
        target.push(char::from(HEX[usize::from(byte & 15)]));
    }
}

impl GroupOutbox {
    pub(super) fn new(inner: Arc<dyn OutboxWriter>, journal: Option<&str>) -> Self {
        let prefix = match journal {
            Some(journal) => {
                let mut prefix = String::from("drasi_group_v1_journal_");
                append_hex(&mut prefix, journal);
                prefix.push('_');
                prefix
            }
            None => String::from("drasi_group_v1_processor_"),
        };
        Self { inner, prefix }
    }

    fn key(&self, query_id: &str) -> Result<String, IndexError> {
        if query_id.is_empty() || query_id.contains('\0') {
            return Err(IndexError::other(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "shared output identity must be nonempty and contain no NUL",
            )));
        }
        let mut key = self.prefix.clone();
        append_hex(&mut key, query_id);
        Ok(key)
    }
}

#[async_trait]
impl OutboxWriter for GroupOutbox {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        self.inner
            .append(&self.key(query_id)?, sequence, data)
            .await
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner
            .read_from(&self.key(query_id)?, after_sequence)
            .await
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(&self.key(query_id)?).await
    }

    async fn read_page(
        &self,
        query_id: &str,
        after_sequence: u64,
        limits: OutboxPageLimits,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner
            .read_page(&self.key(query_id)?, after_sequence, limits)
            .await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.inner.clear(&self.key(query_id)?).await
    }

    async fn append_and_trim(
        &self,
        query_id: &str,
        sequence: u64,
        data: &[u8],
        retain_from: u64,
    ) -> Result<usize, IndexError> {
        self.inner
            .append_and_trim(&self.key(query_id)?, sequence, data, retain_from)
            .await
    }

    async fn trim_before(&self, query_id: &str, retain_from: u64) -> Result<usize, IndexError> {
        self.inner
            .trim_before(&self.key(query_id)?, retain_from)
            .await
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        self.inner
            .trim_to_capacity(&self.key(query_id)?, capacity)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::in_memory_index::in_memory_outbox_writer::InMemoryOutboxWriter;

    #[test]
    fn group_outbox_namespaces_are_disjoint_and_stable() {
        let inner = Arc::new(InMemoryOutboxWriter::new());
        let processor = GroupOutbox::new(inner.clone(), None);
        let journal = GroupOutbox::new(inner.clone(), Some("ab"));
        assert_eq!(processor.key("q").unwrap(), "drasi_group_v1_processor_71");
        assert_eq!(journal.key("q").unwrap(), "drasi_group_v1_journal_6162_71");
        let mut keys = BTreeSet::new();
        for name in [None, Some("a"), Some("ab"), Some("a_b"), Some("\u{e9}")] {
            let writer = GroupOutbox::new(inner.clone(), name);
            for key in ["a", "ab", "b_c", "\u{e9}", "drasi_group_v1_processor_71"] {
                assert!(keys.insert(writer.key(key).unwrap()));
            }
        }
    }

    #[tokio::test]
    async fn group_outbox_rejects_invalid_keys_on_every_operation() {
        let writer = GroupOutbox::new(Arc::new(InMemoryOutboxWriter::new()), None);
        let limits = OutboxPageLimits {
            max_records: std::num::NonZeroUsize::new(1).unwrap(),
            max_bytes: std::num::NonZeroUsize::new(1).unwrap(),
        };
        for key in ["", "\0", "a\0b"] {
            assert!(writer.append(key, 1, b"data").await.is_err());
            assert!(writer.read_from(key, 0).await.is_err());
            assert!(writer.read_page(key, 0, limits).await.is_err());
            assert!(writer.read_latest_sequence(key).await.is_err());
            assert!(writer.clear(key).await.is_err());
            assert!(writer.append_and_trim(key, 1, b"data", 1).await.is_err());
            assert!(writer.trim_before(key, 1).await.is_err());
            assert!(writer.trim_to_capacity(key, 1).await.is_err());
        }
    }

    #[tokio::test]
    async fn group_outbox_pages_preserve_namespace_and_limits() {
        let inner = Arc::new(InMemoryOutboxWriter::new());
        let processor = GroupOutbox::new(inner.clone(), None);
        let journal = GroupOutbox::new(inner, Some("q"));
        processor.append("q", 1, b"processor").await.unwrap();
        journal.append("q", 1, b"journal").await.unwrap();
        journal.append("q", 2, b"next").await.unwrap();
        let limits = OutboxPageLimits {
            max_records: std::num::NonZeroUsize::new(1).unwrap(),
            max_bytes: std::num::NonZeroUsize::new(1).unwrap(),
        };
        assert_eq!(
            processor.read_page("q", 0, limits).await.unwrap(),
            vec![(1, b"processor".to_vec())]
        );
        assert_eq!(
            journal.read_page("q", 0, limits).await.unwrap(),
            vec![(1, b"journal".to_vec())]
        );
        assert_eq!(
            journal.read_page("q", 1, limits).await.unwrap(),
            vec![(2, b"next".to_vec())]
        );
        assert!(journal.read_page("q", 2, limits).await.unwrap().is_empty());
    }
}
