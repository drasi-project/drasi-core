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
    /// Fallible scope entry for local integrations whose execution lease can
    /// retire between polls. A failure must not poll work outside its scope.
    fn in_scope_result(&self, work: &mut dyn FnMut()) -> Result<(), crate::interface::IndexError> {
        self.in_scope(work);
        Ok(())
    }
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
        if let Err(error) =
            observation.in_scope_result(&mut || polled = Some(future.as_mut().poll(cx)))
        {
            return std::task::Poll::Ready(Err(error.into()));
        }
        polled.expect("observation polls its operation exactly once")
    })
    .await;
    observation.finish(result.is_ok(), result.as_ref().ok().and_then(result_count));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::IndexError;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct RetiredObserver(Arc<AtomicBool>);

    impl QueryObserver for RetiredObserver {
        fn start(
            &self,
            _: QueryOperation,
            _: Option<usize>,
        ) -> Result<Box<dyn QueryObservation>, IndexError> {
            Ok(Box::new(RetiredObservation(self.0.clone())))
        }
    }

    struct RetiredObservation(Arc<AtomicBool>);

    impl QueryObservation for RetiredObservation {
        fn in_scope(&self, _: &mut dyn FnMut()) {
            panic!("fallible entry must be used");
        }
        fn in_scope_result(&self, _: &mut dyn FnMut()) -> Result<(), IndexError> {
            Err(IndexError::NotSupported)
        }
        fn finish(&mut self, success: bool, _: Option<usize>) {
            assert!(!success);
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn failed_scope_entry_returns_an_error_without_polling_the_operation() {
        let finished = Arc::new(AtomicBool::new(false));
        let observer: Arc<dyn QueryObserver> = Arc::new(RetiredObserver(finished.clone()));
        let result: Result<(), IndexError> = observe(
            Some(&observer),
            QueryOperation::Evaluate,
            None,
            async { panic!("retired scope must not poll work") },
            |_| None,
        )
        .await;
        assert!(matches!(result, Err(IndexError::NotSupported)));
        assert!(finished.load(Ordering::Acquire));
    }
}
