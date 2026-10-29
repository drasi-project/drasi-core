// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use futures::poll;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

struct Completion(Arc<AtomicUsize>);

impl Drop for Completion {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn cancelled_before_admission() -> Result<()> {
    let (commands, mut receive) = mpsc::channel(1);
    let completed = Arc::new(AtomicUsize::new(0));
    let driver = tokio::spawn({
        let completed = completed.clone();
        async move {
            let _completion = Completion(completed);
            let Some(Command::Close(reply)) = receive.recv().await else {
                panic!("expected exactly one close request");
            };
            reply.send(Ok(())).unwrap();
        }
    });
    let mut task = ManagementTask::new(driver);
    let permit = commands.reserve().await?;
    {
        let mut closing = Box::pin(task.close(&commands));
        assert!(poll!(closing.as_mut()).is_pending());
    }
    assert!(matches!(task.close, CloseProgress::Open));
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    drop(permit);
    task.close(&commands).await?;
    task.close(&commands).await?;
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn cancelled_after_acknowledgement() -> Result<()> {
    let (commands, mut receive) = mpsc::channel(1);
    let acknowledged = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicUsize::new(0));
    let driver = tokio::spawn({
        let acknowledged = acknowledged.clone();
        let release = release.clone();
        let completed = completed.clone();
        async move {
            let _completion = Completion(completed);
            let Some(Command::Close(reply)) = receive.recv().await else {
                panic!("expected a close request");
            };
            reply.send(Ok(())).unwrap();
            acknowledged.notify_one();
            release.notified().await;
        }
    });
    let mut task = ManagementTask::new(driver);
    {
        let mut closing = Box::pin(task.close(&commands));
        assert!(poll!(closing.as_mut()).is_pending());
        acknowledged.notified().await;
        assert!(poll!(closing.as_mut()).is_pending());
    }
    assert!(matches!(task.close, CloseProgress::Joining(None)));
    assert!(!task.driver.is_finished());
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    {
        let mut retry = Box::pin(task.close(&commands));
        assert!(poll!(retry.as_mut()).is_pending());
    }
    release.notify_one();
    task.close(&commands).await?;
    task.close(&commands).await?;
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn concurrent_callers_share_close() -> Result<()> {
    let (commands, mut receive) = mpsc::channel(1);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicUsize::new(0));
    let driver = tokio::spawn({
        let entered = entered.clone();
        let release = release.clone();
        let completed = completed.clone();
        async move {
            let _completion = Completion(completed);
            let Some(Command::Close(reply)) = receive.recv().await else {
                panic!("expected a close request");
            };
            entered.notify_one();
            release.notified().await;
            reply.send(Ok(())).unwrap();
        }
    });
    let task = Arc::new(Mutex::new(ManagementTask::new(driver)));
    let mut calls = Vec::new();
    for _ in 0..3 {
        let task = task.clone();
        let commands = commands.clone();
        calls.push(tokio::spawn(async move {
            task.lock().await.close(&commands).await
        }));
    }
    entered.notified().await;
    assert!(calls.iter().all(|call| !call.is_finished()));
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    release.notify_one();
    for call in calls {
        call.await??;
    }
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn driver_failure_is_retained() -> Result<()> {
    for acknowledge in [false, true] {
        let (commands, mut receive) = mpsc::channel(1);
        let driver = tokio::spawn(async move {
            let Some(Command::Close(reply)) = receive.recv().await else {
                panic!("expected a close request");
            };
            if acknowledge {
                reply.send(Ok(())).unwrap();
            }
            panic!("injected management driver panic");
        });
        let mut task = ManagementTask::new(driver);
        for _ in 0..2 {
            let error = task.close(&commands).await.unwrap_err();
            let panic = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<tokio::task::JoinError>())
                .expect("the driver panic must remain inspectable on every retry");
            assert!(panic.is_panic());
            assert!(format!("{error:#}").contains("injected management driver panic"));
        }
    }

    let (commands, receive) = mpsc::channel(1);
    let driver = tokio::spawn(async move { drop(receive) });
    commands.closed().await;
    let mut task = ManagementTask::new(driver);
    for _ in 0..2 {
        let error = task.close(&commands).await.unwrap_err();
        assert!(error.chain().any(|cause| matches!(
            cause.downcast_ref::<ManagementError>(),
            Some(ManagementError::Closed)
        )));
    }
    Ok(())
}

async fn verify_close_boundaries() -> Result<()> {
    cancelled_before_admission().await?;
    cancelled_after_acknowledgement().await?;
    concurrent_callers_share_close().await?;
    driver_failure_is_retained().await
}

#[tokio::test(flavor = "current_thread")]
async fn management_close_owns_request_and_join_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), verify_close_boundaries()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn management_close_owns_request_and_join_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), verify_close_boundaries()).await?
}
