// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::proxy::{native_operation_cause, RemoteComponent};
use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_plugin_abi::{self as abi, source_schema::*};
use drasi_computation_plugin_sdk::transport::{take_reply, take_status, OperationFuture};
use drasi_lib::computation::v1::{decode_bounded_messagepack, ComponentRole, SourceSchemaReader};
use std::sync::{Arc, Weak};
use tokio::sync::watch;

pub(super) struct NativeSourceSchemaReader {
    component: Weak<RemoteComponent>,
    extension: PluginSourceSchemaV1,
    retired: watch::Sender<bool>,
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
        })))
    }

    pub(super) fn retire(&self) {
        self.retired.send_replace(true);
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
        let bytes = tokio::select! {
            biased;
            _ = retired.changed() => anyhow::bail!("native source schema owner was retired"),
            result = operation => result.map_err(native_operation_cause)?,
        };
        self.check_active()?;
        Ok(decode_bounded_messagepack(&bytes, MAX_SCHEMA_BYTES)?)
    }
}
