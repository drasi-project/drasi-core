// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{encode_bounded_messagepack, SourceSchemaReader};
use std::{
    sync::{atomic::AtomicU64, Mutex},
    time::Duration,
};

pub(super) static EXTENSION: abi::source_schema::PluginSourceSchemaV1 =
    abi::source_schema::PluginSourceSchemaV1 {
        header: Header::new::<abi::source_schema::PluginSourceSchemaV1>(),
        version: abi::source_schema::VERSION,
        reserved: 0,
        supported: Some(supported),
        begin: Some(begin),
    };

pub(super) struct SchemaState {
    reader: Arc<dyn SourceSchemaReader>,
    lifecycle: Mutex<(u64, bool)>,
    busy: AtomicU64,
    changed: tokio::sync::watch::Sender<()>,
}

impl SchemaState {
    pub(super) fn new(reader: Arc<dyn SourceSchemaReader>) -> Arc<Self> {
        Arc::new(Self {
            reader,
            lifecycle: Mutex::new((0, false)),
            busy: AtomicU64::new(0),
            changed: tokio::sync::watch::channel(()).0,
        })
    }

    pub(super) fn activate(&self) -> anyhow::Result<()> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("native source schema lifecycle lock is poisoned"))?;
        lifecycle.0 = lifecycle
            .0
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native source schema generation exhausted"))?;
        lifecycle.1 = true;
        self.changed.send_modify(|_| ());
        Ok(())
    }

    pub(super) fn deactivate(&self) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        lifecycle.1 = false;
        self.busy.store(0, Ordering::Release);
        self.changed.send_modify(|_| ());
    }

    fn claim(self: &Arc<Self>) -> Result<(ReadGuard, tokio::sync::watch::Receiver<()>), Failure> {
        let lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| Failure::protocol("native source schema lifecycle lock is poisoned"))?;
        if !lifecycle.1 {
            return Err(Failure::new(
                abi::status::CLOSED,
                "native source schema is not active",
            ));
        }
        self.busy
            .compare_exchange(0, lifecycle.0, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                Failure::new(
                    abi::status::BUSY,
                    "native source schema read is already active",
                )
            })?;
        Ok((
            ReadGuard {
                state: self.clone(),
                generation: lifecycle.0,
            },
            self.changed.subscribe(),
        ))
    }
}

struct ReadGuard {
    state: Arc<SchemaState>,
    generation: u64,
}

impl ReadGuard {
    fn validate(&self) -> Result<(), Failure> {
        let lifecycle =
            self.state.lifecycle.lock().map_err(|_| {
                Failure::protocol("native source schema lifecycle lock is poisoned")
            })?;
        if lifecycle.1 && lifecycle.0 == self.generation {
            Ok(())
        } else {
            Err(Failure::new(
                abi::status::CLOSED,
                "native source schema read belongs to a retired activation",
            ))
        }
    }
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        let _ = self.state.busy.compare_exchange(
            self.generation,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

unsafe extern "C" fn supported(raw: *mut c_void) -> Reply {
    transport::reply_boundary(|| {
        let instance = unsafe { state_ref::<Arc<Instance>>(raw)? };
        if instance.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        wire::encode(&instance.source_schema.is_some()).map_err(Failure::from)
    })
}

unsafe extern "C" fn begin(raw: *mut c_void, out: *mut abi::OperationHandle) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let instance = unsafe { state_ref::<Arc<Instance>>(raw)? }.clone();
        if instance.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let state = instance.source_schema.as_ref().ok_or_else(|| {
            Failure::new(
                abi::status::UNSUPPORTED,
                "native source has no data schema reader",
            )
        })?;
        if let Some(coordinator) = &instance.coordinator {
            coordinator.check_active().map_err(Failure::from)?;
        }
        let (guard, mut changed) = state.claim()?;
        let runtime = io_runtime()?;
        let damaged = instance.damaged.clone();
        let operation = transport::export_component_operation(
            async move {
                guard.validate()?;
                let schema = tokio::select! {
                    biased;
                    _ = changed.changed() => return Err(Failure::new(
                        abi::status::CLOSED,
                        "native source schema activation was retired",
                    )),
                    result = tokio::time::timeout(
                        Duration::from_secs(30),
                        guard.state.reader.describe_schema(),
                    ) => result.map_err(|_| {
                        Failure::new(abi::status::FAILED, "native source schema read timed out")
                    })?.map_err(Failure::from)?,
                };
                let bytes =
                    encode_bounded_messagepack(&schema, abi::source_schema::MAX_SCHEMA_BYTES)
                        .map_err(|error| Failure::from(anyhow::Error::from(error)))?;
                guard.validate()?;
                if let Some(coordinator) = &instance.coordinator {
                    coordinator.check_active().map_err(Failure::from)?;
                }
                Ok(bytes)
            },
            Some(runtime),
            damaged,
            false,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::schema::{NodeSchema, SourceSchema};

    struct EmptyReader;

    #[async_trait::async_trait]
    impl SourceSchemaReader for EmptyReader {
        async fn describe_schema(&self) -> anyhow::Result<Option<SourceSchema>> {
            Ok(None)
        }
    }

    #[test]
    fn retired_read_guard_cannot_release_a_new_activation_slot() -> anyhow::Result<()> {
        let state = SchemaState::new(Arc::new(EmptyReader));
        assert!(state.claim().is_err());
        state.activate()?;
        let (old, changed) = state.claim()?;
        assert!(state.claim().is_err());
        state.deactivate();
        assert!(changed.has_changed()?);
        state.activate()?;
        let (current, _) = state.claim()?;
        assert!(old.validate().is_err());
        drop(old);
        assert!(state.claim().is_err());
        current.validate()?;
        drop(current);
        assert!(state.claim().is_ok());
        Ok(())
    }

    #[test]
    fn source_schema_byte_limit_covers_the_complete_reply() -> anyhow::Result<()> {
        let maximum = abi::source_schema::MAX_SCHEMA_BYTES;
        let schema = |length| {
            Some(SourceSchema {
                nodes: vec![NodeSchema::new("x".repeat(length))],
                relations: Vec::new(),
            })
        };
        let sample_length = maximum - 128;
        let overhead =
            encode_bounded_messagepack(&schema(sample_length), maximum)?.len() - sample_length;
        assert_eq!(
            encode_bounded_messagepack(&schema(maximum - overhead), maximum)?.len(),
            maximum
        );
        assert!(encode_bounded_messagepack(&schema(maximum - overhead + 1), maximum).is_err());
        Ok(())
    }
}
