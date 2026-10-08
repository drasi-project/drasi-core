// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::models::SourceChange;
use drasi_lib::context::workers::WorkerCleanupError;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

fn file_source(directory: &TempDir, rest: bool) -> Result<SqliteSource> {
    let mut builder = SqliteSource::builder("sqlite-lifecycle").with_path(
        directory
            .path()
            .join("data.db")
            .to_string_lossy()
            .into_owned(),
    );
    if rest {
        builder = builder.with_rest_api(RestApiConfig {
            host: "127.0.0.1".into(),
            port: 0,
        });
    }
    builder.build()
}

#[tokio::test(flavor = "current_thread")]
async fn database_and_listener_start_failures_retain_typed_causes() -> Result<()> {
    let directory = TempDir::new()?;
    let missing = directory.path().join("missing");
    let source = SqliteSource::builder("bad-database")
        .with_path(missing.join("data.db").to_string_lossy().into_owned())
        .build()?;
    let error = source.start().await.unwrap_err();
    assert!(error.is::<rusqlite::Error>());
    assert_eq!(source.status().await, ComponentStatus::Error);
    assert!(source.database_worker.read().await.is_none());
    assert!(source.command_tx.read().await.is_none());
    assert!(source.start().await.unwrap_err().is::<WorkerAlreadyOwned>());
    source.stop().await?;
    std::fs::create_dir(&missing)?;
    source.start().await?;
    source.handle().query("SELECT 1").await?;
    source.stop().await?;

    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = occupied.local_addr()?;
    let source = SqliteSource::builder("bad-listener")
        .with_rest_api(RestApiConfig {
            host: "127.0.0.1".into(),
            port: address.port(),
        })
        .build()?;
    assert!(source.start().await.unwrap_err().is::<std::io::Error>());
    assert_eq!(source.status().await, ComponentStatus::Error);
    assert!(source.database_worker.read().await.is_some());
    assert!(source.rest_worker.read().await.is_none());
    assert!(source.command_tx.read().await.is_none());
    assert!(source.start().await.unwrap_err().is::<WorkerAlreadyOwned>());
    source.stop().await?;
    assert!(source.database_worker.read().await.is_none());
    drop(occupied);
    source.start().await?;
    assert_eq!(source.bound_rest_address().await, Some(address));
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_start_and_panicked_database_keep_cleanup_required() -> Result<()> {
    let source = SqliteSource::builder("cancelled-start").build()?;
    let public_sender = source.command_tx.write().await;
    {
        let start = source.start();
        tokio::pin!(start);
        timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut start => panic!("public admission is still locked: {result:?}"),
                _ = async {
                    while source.base.task_handle.read().await.is_none() {
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        })
        .await?;
    }
    assert!(public_sender.is_none());
    assert!(source.database_worker.read().await.is_some());
    drop(public_sender);
    assert!(source.start().await.unwrap_err().is::<WorkerAlreadyOwned>());
    source.stop().await?;

    spawn_owned_blocking_worker(&source.database_worker, || panic!("database panic")).await?;
    let error = source.stop().await.unwrap_err();
    assert!(
        matches!(error.downcast_ref::<WorkerCleanupError>(), Some(WorkerCleanupError::Join(cause)) if cause.is_panic())
    );
    assert!(source.database_worker.read().await.is_none());
    let base_cleanup = source.base.shutdown_tx.write().await;
    assert!(timeout(Duration::from_millis(20), source.stop())
        .await
        .is_err());
    assert!(source.start().await.unwrap_err().is::<WorkerAlreadyOwned>());
    drop(base_cleanup);
    source.stop().await?;
    source.start().await?;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn submitted_sqlite_write_finishes_after_cancelled_and_timed_out_stop() -> Result<()> {
    let directory = TempDir::new()?;
    let source = file_source(&directory, false)?;
    let handle = source.handle();
    let mut receiver = source.base.try_test_subscribe().await?;
    source.start().await?;
    handle
        .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY); PRAGMA busy_timeout=30000;")
        .await?;
    let blocker = rusqlite::Connection::open(directory.path().join("data.db"))?;
    blocker.execute_batch("BEGIN IMMEDIATE")?;
    let write = handle.execute("INSERT INTO items VALUES(1)");
    tokio::pin!(write);
    assert!(timeout(Duration::from_millis(20), &mut write)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(20), source.stop())
        .await
        .is_err());
    let stopped_at = tokio::time::Instant::now();
    let error = source.stop().await.unwrap_err();
    assert!(stopped_at.elapsed() >= Duration::from_secs(5));
    assert!(matches!(
        error.downcast_ref::<WorkerCleanupError>(),
        Some(WorkerCleanupError::TimedOut { .. })
    ));
    assert!(!source
        .database_worker
        .read()
        .await
        .as_ref()
        .unwrap()
        .is_finished());
    assert_eq!(source.status().await, ComponentStatus::Stopping);
    assert_eq!(
        handle.query("SELECT 1").await.unwrap_err().to_string(),
        "source 'sqlite-lifecycle' is not running"
    );
    assert!(source.start().await.unwrap_err().is::<WorkerAlreadyOwned>());

    blocker.execute_batch("COMMIT")?;
    assert_eq!(timeout(Duration::from_secs(2), &mut write).await??, 1);
    source.stop().await?;
    let event = timeout(Duration::from_secs(1), receiver.recv()).await??;
    assert!(matches!(
        &event.event,
        SourceEvent::Change(SourceChange::Insert { .. })
    ));
    assert_eq!(
        timeout(Duration::from_secs(1), receiver.recv())
            .await?
            .unwrap_err()
            .to_string(),
        "Channel closed"
    );
    assert!(source.database_worker.read().await.is_none());
    assert!(source.base.task_handle.read().await.is_none());
    source.start().await?;
    assert_eq!(
        serde_json::to_value(handle.query("SELECT id FROM items").await?)?,
        serde_json::json!([{"id": 1}])
    );
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_http_body_retains_the_server_and_database_until_completion() -> Result<()> {
    let directory = TempDir::new()?;
    let source = file_source(&directory, true)?;
    let handle = source.handle();
    source.start().await?;
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let address = source
        .bound_rest_address()
        .await
        .expect("bound REST listener");
    let mut request = tokio::net::TcpStream::connect(address).await?;
    let body = r#"{"id":1}"#;
    request.write_all(format!(
        "POST /api/tables/items HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n", body.len(),
    ).as_bytes()).await?;
    let mut interim = [0; 25];
    timeout(Duration::from_secs(2), request.read_exact(&mut interim)).await??;
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    assert!(timeout(Duration::from_millis(20), source.stop())
        .await
        .is_err());
    let stopped_at = tokio::time::Instant::now();
    let error = source.stop().await.unwrap_err();
    assert!(stopped_at.elapsed() >= Duration::from_secs(5));
    assert!(matches!(
        error.downcast_ref::<WorkerCleanupError>(),
        Some(WorkerCleanupError::TimedOut { .. })
    ));
    assert!(!source
        .rest_worker
        .read()
        .await
        .as_ref()
        .unwrap()
        .is_finished());
    assert!(!source
        .database_worker
        .read()
        .await
        .as_ref()
        .unwrap()
        .is_finished());
    assert!(source.database_commands.read().await.is_some());
    assert!(source.command_tx.read().await.is_none());
    assert!(source.start().await.unwrap_err().is::<WorkerAlreadyOwned>());

    let late_body = r#"{"id":2}"#;
    let completion_and_late_request = format!(
        "{body}POST /api/tables/items HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{late_body}", late_body.len(),
    );
    request
        .write_all(completion_and_late_request.as_bytes())
        .await?;
    let mut response = String::new();
    timeout(
        Duration::from_secs(2),
        request.read_to_string(&mut response),
    )
    .await??;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains(r#"{"success":true}"#), "{response}");
    source.stop().await?;
    assert!(source.rest_worker.read().await.is_none());
    assert!(source.bound_rest_address().await.is_none());
    assert_eq!(source.shutdown_tx.receiver_count(), 0);
    let listener = tokio::net::TcpListener::bind(address).await?;
    drop(listener);
    source.start().await?;
    assert_eq!(
        serde_json::to_value(handle.query("SELECT id FROM items").await?)?,
        serde_json::json!([{"id": 1}])
    );
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_continuations_cannot_write_to_a_replacement_worker() -> Result<()> {
    let directory = TempDir::new()?;
    let source = file_source(&directory, false)?;
    let handle = source.handle();
    source.start().await?;
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, active) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    let transaction = handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered
            .send(tx.clone())
            .map_err(|_| anyhow!("transaction observer closed"))?;
        resume.await?;
        tx.execute("INSERT INTO items VALUES(2)").await?;
        Ok(())
    });
    tokio::pin!(transaction);
    let captured = timeout(Duration::from_secs(2), async {
        tokio::select! {
            tx = active => tx,
            result = &mut transaction => panic!("transaction unexpectedly completed: {result:?}"),
        }
    })
    .await??;
    source.stop().await?;
    source.start().await?;
    assert!(handle.query("SELECT id FROM items").await?.is_empty());
    assert!(captured
        .execute("INSERT INTO items VALUES(9)")
        .await
        .unwrap_err()
        .is::<mpsc::error::SendError<SqliteCommand>>());
    release.send(()).expect("transaction still waiting");
    assert!(transaction
        .await
        .unwrap_err()
        .is::<mpsc::error::SendError<SqliteCommand>>());
    assert!(handle.query("SELECT id FROM items").await?.is_empty());
    handle.execute("INSERT INTO items VALUES(3)").await?;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn three_database_and_rest_restarts_release_all_previous_owners() -> Result<()> {
    let directory = TempDir::new()?;
    let source = file_source(&directory, true)?;
    let handle = source.handle();
    let client = reqwest::Client::new();
    source.stop().await?;
    for id in 1..=3 {
        let mut receiver = source.base.try_test_subscribe().await?;
        source.start().await?;
        source.start().await?;
        handle
            .execute("CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY)")
            .await?;
        let address = source
            .bound_rest_address()
            .await
            .expect("bound REST listener");
        let response = client
            .post(format!("http://{address}/api/tables/items"))
            .json(&serde_json::json!({"id":id}))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await?,
            serde_json::json!({"success":true})
        );
        source.stop().await?;
        let event = timeout(Duration::from_secs(1), receiver.recv()).await??;
        assert!(matches!(
            &event.event,
            SourceEvent::Change(SourceChange::Insert { .. })
        ));
        assert_eq!(
            timeout(Duration::from_secs(1), receiver.recv())
                .await?
                .unwrap_err()
                .to_string(),
            "Channel closed"
        );
        assert!(source.database_worker.read().await.is_none());
        assert!(source.rest_worker.read().await.is_none());
        assert!(source.base.task_handle.read().await.is_none());
        assert_eq!(source.shutdown_tx.receiver_count(), 0);
        assert!(source.bound_rest_address().await.is_none());
        let listener = tokio::net::TcpListener::bind(address).await?;
        drop(listener);
    }
    source.stop().await?;
    Ok(())
}
