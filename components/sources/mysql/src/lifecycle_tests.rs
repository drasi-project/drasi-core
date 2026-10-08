// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::context::workers::WorkerCleanupError;
use tokio::time::timeout;

fn source(port: u16) -> Result<MySqlReplicationSource> {
    MySqlReplicationSource::builder("mysql-lifecycle")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_database("test")
        .with_user("test")
        .with_ssl_mode(SslMode::Disabled)
        .build()
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_registration_and_subscriber_wait_keep_one_owned_generation() -> Result<()> {
    let source = source(3306)?;
    source.stop().await?;
    {
        let slot = source.replication_task.read().await;
        let start = source.start();
        tokio::pin!(start);
        assert!(futures_util::poll!(&mut start).is_pending());
        assert!(slot.is_none());
        assert_eq!(source.status().await, ComponentStatus::Starting);
    }
    assert!(source
        .start()
        .await
        .expect_err("cancelled registration")
        .downcast_ref::<WorkerAlreadyOwned>()
        .is_some());
    source.stop().await?;
    for _ in 0..3 {
        source.start().await?;
        source.start().await?;
        assert_eq!(source.status().await, ComponentStatus::Running);
        assert!(source
            .replication_task
            .read()
            .await
            .as_ref()
            .is_some_and(|task| !task.is_finished()));
        timeout(Duration::from_secs(1), source.stop()).await??;
        assert!(source.replication_task.read().await.is_none());
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }
    source.start().await?;
    let positions = source.subscriber_resume_positions.read().await;
    {
        let stop = source.stop();
        tokio::pin!(stop);
        tokio::select! {
            result = &mut stop => anyhow::bail!("subscriber cleanup is still blocked: {result:?}"),
            result = timeout(Duration::from_secs(1), async {
                while source.replication_task.read().await.is_some() {
                    tokio::task::yield_now().await;
                }
            }) => result?,
        }
    }
    assert_eq!(source.status().await, ComponentStatus::Stopping);
    assert!(source
        .start()
        .await
        .expect_err("joined worker still needs base cleanup")
        .downcast_ref::<WorkerAlreadyOwned>()
        .is_some());
    drop(positions);
    source.stop().await?;
    assert_eq!(source.status().await, ComponentStatus::Stopped);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_and_panicked_replication_require_cleanup_even_in_error_status() -> Result<()> {
    for panic in [false, true] {
        let source = source(3306)?;
        *source.cleanup_required.lock().await = true;
        source.subscriber_resume_positions.write().await.insert(
            "old-query".into(),
            ReplicationState {
                binlog_file: "mysql-bin.000001".into(),
                binlog_position: 100,
                gtid_set: None,
                last_processed_timestamp: 0,
            },
        );
        source
            .base
            .set_status(
                ComponentStatus::Error,
                Some("injected worker failure".into()),
            )
            .await;
        spawn_owned_worker(&source.replication_task, async move {
            assert!(!panic, "injected MySQL worker panic");
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "injected replication failure",
            )
            .into())
        })
        .await?;
        let error = source.stop().await.expect_err("typed worker failure");
        if panic {
            assert!(
                matches!(error.downcast_ref::<WorkerCleanupError>(), Some(WorkerCleanupError::Join(error)) if error.is_panic())
            );
        } else {
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .context("original I/O cause")?
                    .kind(),
                std::io::ErrorKind::ConnectionReset
            );
        }
        assert!(source.replication_task.read().await.is_none());
        assert!(!source.subscriber_resume_positions.read().await.is_empty());
        assert!(source
            .start()
            .await
            .expect_err("cleanup is incomplete")
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        source.stop().await?;
        assert!(source.subscriber_resume_positions.read().await.is_empty());
        source.start().await?;
        source.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn unfinished_mysql_handshake_survives_cancelled_and_timed_out_cleanup() -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let accepted = StdArc::new(tokio::sync::Notify::new());
    let release = StdArc::new(tokio::sync::Notify::new());
    let peer = tokio::spawn({
        let accepted = accepted.clone();
        let release = release.clone();
        async move {
            let (connection, _) = listener.accept().await?;
            accepted.notify_one();
            release.notified().await;
            drop(connection);
            Result::<()>::Ok(())
        }
    });
    let source = source(address.port())?;
    let mut receiver = source.base.try_test_subscribe().await?;
    source.start().await?;
    timeout(Duration::from_secs(3), accepted.notified()).await?;
    {
        let stop = source.stop();
        tokio::pin!(stop);
        tokio::select! {
            result = &mut stop => anyhow::bail!("unfinished handshake returned early: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
    assert!(source
        .start()
        .await
        .expect_err("old handshake owner")
        .downcast_ref::<WorkerAlreadyOwned>()
        .is_some());
    let error = source.stop().await.expect_err("unfinished handshake");
    assert!(
        matches!(error.downcast_ref::<WorkerCleanupError>(), Some(WorkerCleanupError::TimedOut { timeout }) if *timeout == Duration::from_secs(5)),
        "{error:#}"
    );
    assert!(source
        .replication_task
        .read()
        .await
        .as_ref()
        .is_some_and(|task| !task.is_finished()));
    release.notify_one();
    timeout(Duration::from_secs(2), peer).await???;
    let error = timeout(Duration::from_secs(3), source.stop())
        .await?
        .expect_err("original connection failure");
    assert!(
        error.downcast_ref::<mysql_async::Error>().is_some(),
        "{error:#}"
    );
    assert!(source.replication_task.read().await.is_none());
    source.stop().await?;
    assert!(timeout(Duration::from_secs(1), receiver.recv())
        .await?
        .is_err());
    source.start().await?;
    source.stop().await?;
    Ok(())
}
