// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::proxy::{native_operation_cause, RemoteComponent};
use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_plugin_abi::{self as abi, source_schema::*};
use drasi_computation_plugin_sdk::transport::{
    take_reply, take_status, Failure, OperationCancellation, OperationFuture,
};
use drasi_lib::computation::v1::{decode_bounded_messagepack, ComponentRole, SourceSchemaReader};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::watch;

pub(super) struct NativeSourceSchemaReader {
    component: Weak<RemoteComponent>,
    extension: PluginSourceSchemaV1,
    retired: watch::Sender<bool>,
    generation: watch::Sender<u64>,
    operations: Mutex<Vec<OperationCancellation>>,
}

impl NativeSourceSchemaReader {
    pub(super) fn bind(
        component: &Arc<RemoteComponent>,
        extension: PluginSourceSchemaV1,
        role: ComponentRole,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let bytes = unsafe {
            take_reply(extension.supported.expect("validated")(component.state()))
                .map_err(native_operation_cause)?
        };
        let supported: bool = decode_bounded_messagepack(&bytes, 16)?;
        if !supported {
            return Ok(None);
        }
        anyhow::ensure!(
            role == ComponentRole::Source,
            "native data schema reader requires a source component"
        );
        Ok(Some(Arc::new(Self {
            component: Arc::downgrade(component),
            extension,
            retired: watch::channel(false).0,
            generation: watch::channel(0).0,
            operations: Mutex::new(Vec::new()),
        })))
    }

    pub(super) fn advance(&self) {
        self.cancel_operations();
        self.generation.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }

    pub(super) fn retire(&self) {
        self.cancel_operations();
        self.retired.send_replace(true);
    }

    fn track(&self, operation: &OperationFuture) {
        let mut operations = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        operations.retain(OperationCancellation::is_active);
        operations.push(operation.cancellation());
    }

    fn cancel_operations(&self) {
        let operations = std::mem::take(
            &mut *self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for operation in operations {
            let _ = operation.cancel();
        }
    }

    fn check_active(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !*self.retired.borrow(),
            "native source schema owner was retired"
        );
        Ok(())
    }
}

#[async_trait]
impl SourceSchemaReader for NativeSourceSchemaReader {
    async fn describe_schema(&self) -> anyhow::Result<Option<drasi_lib::schema::SourceSchema>> {
        self.check_active()?;
        let mut retired = self.retired.subscribe();
        let mut generation = self.generation.subscribe();
        generation.borrow_and_update();
        self.check_active()?;
        let component = self
            .component
            .upgrade()
            .context("native source schema owner was retired")?;
        let operation = unsafe {
            let mut raw = abi::OperationHandle::null();
            take_status(self.extension.begin.expect("validated")(
                component.state(),
                &mut raw,
            ))
            .map_err(native_operation_cause)?;
            OperationFuture::new(raw).map_err(native_operation_cause)?
        };
        self.track(&operation);
        let bytes = tokio::select! {
            biased;
            _ = retired.changed() => return Err(native_operation_cause(Failure::closed())),
            _ = generation.changed() => return Err(native_operation_cause(Failure::closed())),
            result = operation => result.map_err(native_operation_cause)?,
        };
        self.check_active()?;
        Ok(decode_bounded_messagepack(&bytes, MAX_SCHEMA_BYTES)?)
    }
}
