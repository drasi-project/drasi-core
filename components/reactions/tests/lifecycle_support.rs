// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::context::workers::{WorkerAlreadyOwned, WorkerCleanupError};
use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::{MemoryStateStoreProvider, Reaction, StateStoreProvider, StateStoreResult};
use tokio::sync::{Notify, Semaphore};

#[allow(dead_code)]
pub async fn read_http_request(socket: &mut tokio::net::TcpStream) -> (String, serde_json::Value) {
    use tokio::io::AsyncReadExt;

    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 16_384);
        header.push(socket.read_u8().await.unwrap());
    }
    let header = String::from_utf8(header).unwrap();
    let length = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .unwrap()
        .1
        .trim()
        .parse::<usize>()
        .unwrap();
    assert!(length < 16_384);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    (header, serde_json::from_slice(&body).unwrap())
}

pub fn result(sequence: u64) -> QueryResult {
    QueryResult::new(
        "q1".into(),
        sequence,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: serde_json::json!({"generation": sequence}),
            row_signature: sequence,
        }],
        HashMap::new(),
    )
}

pub async fn initialize(reaction: &dyn Reaction, store: Arc<dyn StateStoreProvider>) {
    let (updates, _receiver) = tokio::sync::mpsc::channel(64);
    reaction
        .initialize(drasi_lib::context::ReactionRuntimeContext::new(
            "lifecycle",
            reaction.id(),
            Some(store),
            updates,
            None,
        ))
        .await;
}

pub async fn assert_incomplete_stop(reaction: &dyn Reaction, base: &ReactionBase) {
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.stop())
            .await
            .is_err()
    );
    assert!(base.processing_task.read().await.is_some());
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    let started = tokio::time::Instant::now();
    let error = reaction.stop().await.unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(matches!(
        error.downcast_ref::<WorkerCleanupError>(),
        Some(WorkerCleanupError::TimedOut { .. })
    ));
    assert!(matches!(reaction.status().await, ComponentStatus::Stopping));
    assert!(!base
        .processing_task
        .read()
        .await
        .as_ref()
        .unwrap()
        .is_finished());
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
}

pub async fn assert_cancelled_start(reaction: &dyn Reaction, base: &ReactionBase) {
    let locked = base.shutdown_tx.write().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.start())
            .await
            .is_err()
    );
    assert!(base.processing_task.read().await.is_none());
    drop(locked);
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    reaction.stop().await.unwrap();
}

pub async fn assert_panic_cleanup(reaction: &dyn Reaction, base: &ReactionBase) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !base
            .processing_task
            .read()
            .await
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let error = reaction.stop().await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<WorkerCleanupError>(),
        Some(WorkerCleanupError::Join(error)) if error.is_panic()
    ));
    assert!(base.processing_task.read().await.is_none());
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    let locked = base.shutdown_tx.write().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.stop())
            .await
            .is_err()
    );
    drop(locked);
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    reaction.stop().await.unwrap();
    reaction.start().await.unwrap();
    reaction.stop().await.unwrap();
}

pub struct ControlledStore {
    inner: MemoryStateStoreProvider,
    pub entered: Notify,
    pub release: Semaphore,
    panic_on_write: bool,
}

impl ControlledStore {
    pub fn new(panic_on_write: bool) -> Self {
        Self {
            inner: MemoryStateStoreProvider::new(),
            entered: Notify::new(),
            release: Semaphore::new(0),
            panic_on_write,
        }
    }
}

#[async_trait::async_trait]
impl StateStoreProvider for ControlledStore {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store_id, key).await
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        assert!(!self.panic_on_write, "injected checkpoint panic");
        self.inner.set(store_id, key, value).await
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        for (key, value) in entries {
            self.set(store_id, key, value.to_vec()).await?;
        }
        Ok(())
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.key_count(store_id).await
    }
}
