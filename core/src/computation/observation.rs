// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{future::Future, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryOperation {
    Evaluate,
    Commit,
}

/// Optional local observation hooks. No evaluator, storage or telemetry ownership
/// crosses this interface, and implementations must not block or alter processing.
pub trait QueryObserver: Send + Sync {
    fn start(
        &self,
        operation: QueryOperation,
        change_count: Option<usize>,
    ) -> Result<Box<dyn QueryObservation>, crate::interface::IndexError>;
}

pub trait QueryObservation: Send + Sync {
    fn in_scope(&self, work: &mut dyn FnMut());
    fn finish(&mut self, success: bool, result_count: Option<usize>);
}

pub(super) async fn observe<T, E: From<crate::interface::IndexError>>(
    observer: Option<&Arc<dyn QueryObserver>>,
    operation: QueryOperation,
    change_count: Option<usize>,
    future: impl Future<Output = Result<T, E>>,
    result_count: impl FnOnce(&T) -> Option<usize>,
) -> Result<T, E> {
    let Some(observer) = observer else {
        return future.await;
    };
    let mut observation = observer.start(operation, change_count)?;
    let mut future = Box::pin(future);
    let result = std::future::poll_fn(|cx| {
        let mut polled = None;
        observation.in_scope(&mut || polled = Some(future.as_mut().poll(cx)));
        polled.expect("observation polls its operation exactly once")
    })
    .await;
    observation.finish(result.is_ok(), result.as_ref().ok().and_then(result_count));
    result
}
