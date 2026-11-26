// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Binds a value, such as a processing context, to the one host operation
//! allowed to use it. A stateful transformer's private stores are reachable
//! only while their operation runs.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};

use drasi_core_types::interface::IndexError;

fn binding_error(message: &'static str) -> IndexError {
    IndexError::other(std::io::Error::other(message))
}

/// A value bound to one active operation.
pub struct BoundContext<T> {
    value: Arc<T>,
    active: AtomicBool,
}

impl<T> BoundContext<T> {
    /// The bound value, while its operation is still active.
    pub fn get(&self) -> Result<Arc<T>, IndexError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(binding_error("transformer operation is no longer active"));
        }
        Ok(self.value.clone())
    }
}

/// Keeps a binding active until it is dropped at the end of its operation.
#[must_use = "retain the guard until the processing operation finishes"]
pub struct ContextGuard<T>(Arc<BoundContext<T>>);

impl<T> Drop for ContextGuard<T> {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
    }
}

/// Holds at most one active operation's value.
pub struct ContextBinding<T>(Mutex<Weak<BoundContext<T>>>);

impl<T> Default for ContextBinding<T> {
    fn default() -> Self {
        Self(Mutex::new(Weak::new()))
    }
}

impl<T> ContextBinding<T> {
    /// Bind `value` for one operation. Fails while another operation is active.
    pub fn bind(&self, value: Arc<T>) -> Result<ContextGuard<T>, IndexError> {
        let mut current = self
            .0
            .lock()
            .map_err(|_| binding_error("transformer operation binding poisoned"))?;
        if current
            .upgrade()
            .is_some_and(|context| context.active.load(Ordering::Acquire))
        {
            return Err(binding_error("transformer already has an active operation"));
        }
        let context = Arc::new(BoundContext {
            value,
            active: AtomicBool::new(true),
        });
        *current = Arc::downgrade(&context);
        Ok(ContextGuard(context))
    }

    /// The active operation's binding.
    pub fn current(&self) -> Result<Arc<BoundContext<T>>, IndexError> {
        self.0
            .lock()
            .map_err(|_| binding_error("transformer operation binding poisoned"))?
            .upgrade()
            .ok_or_else(|| binding_error("transformer has no active operation"))
    }

    /// The active operation's value.
    pub fn get(&self) -> Result<Arc<T>, IndexError> {
        self.current()?.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{pending, Future},
        task::{Context, Waker},
    };

    #[test]
    fn operation_binding_is_exclusive_and_revoked_before_reuse() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        assert!(binding.get().is_err());
        let first = binding.bind(Arc::new(1))?;
        let retired = binding.current()?;
        assert_eq!(*binding.get()?, 1);
        assert!(binding.bind(Arc::new(2)).is_err());
        drop(first);
        assert!(binding.get().is_err());
        assert!(retired.get().is_err());
        let second = binding.bind(Arc::new(2))?;
        assert_eq!(*binding.get()?, 2);
        assert!(retired.get().is_err());
        drop(retired);
        assert_eq!(*binding.get()?, 2);
        drop(second);
        assert!(binding.get().is_err());
        Ok(())
    }

    #[test]
    fn cancelled_operation_releases_its_context() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        let value = Arc::new(1);
        let weak = Arc::downgrade(&value);
        let mut operation = Box::pin(async {
            let _guard = binding.bind(value)?;
            pending::<()>().await;
            Ok::<_, IndexError>(())
        });
        assert!(operation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(*binding.get()?, 1);
        drop(operation);
        assert!(binding.get().is_err());
        assert!(weak.upgrade().is_none());
        let _guard = binding.bind(Arc::new(2))?;
        assert_eq!(*binding.get()?, 2);
        Ok(())
    }

    #[test]
    fn completed_context_is_not_retained_or_rebound_by_lazy_readers() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        let value = Arc::new(1);
        let weak = Arc::downgrade(&value);
        let guard = binding.bind(value)?;
        let reader = Arc::downgrade(&binding.current()?);
        drop(guard);
        assert!(weak.upgrade().is_none());
        assert!(reader.upgrade().is_none());
        let _guard = binding.bind(Arc::new(2))?;
        assert_eq!(*binding.get()?, 2);
        assert!(reader.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn failed_operation_and_other_instances_cannot_reuse_a_binding() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        let other = ContextBinding::default();
        let _other = other.bind(Arc::new(2))?;
        let failure = (|| {
            let _guard = binding.bind(Arc::new(1))?;
            assert_eq!(*binding.get()?, 1);
            assert_eq!(*other.get()?, 2);
            Err::<(), _>(binding_error("injected operation failure"))
        })();
        assert!(failure.is_err());
        assert!(binding.get().is_err());
        assert_eq!(*other.get()?, 2);
        let _guard = binding.bind(Arc::new(3))?;
        assert_eq!(*binding.get()?, 3);
        Ok(())
    }
}
