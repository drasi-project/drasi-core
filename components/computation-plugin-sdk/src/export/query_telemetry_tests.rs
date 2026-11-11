// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::Capabilities;
use drasi_computation_contracts::{
    ComponentRole, ComponentSemanticKind, GraphChangeCodec, ImplementationIdentity, PortDescriptor,
    PortDirection, PortId,
};

struct QueryFactory(bool);

impl Factory for QueryFactory {
    fn metadata(&self) -> FactoryMetadata {
        let schema = GraphChangeCodec::schema();
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("query/capability", "1").unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: Default::default(),
            ports: vec![
                PortDescriptor::new(
                    PortId::try_new("in").unwrap(),
                    PortDirection::Input,
                    schema.descriptor().clone(),
                    Default::default(),
                ),
                PortDescriptor::new(
                    PortId::try_new("out").unwrap(),
                    PortDirection::Output,
                    schema.descriptor().clone(),
                    Default::default(),
                ),
            ],
            completion: None,
            capabilities: Capabilities {
                semantic_kind: Some(ComponentSemanticKind::Query),
                ..Default::default()
            },
        }
    }

    fn supports_query_instrumentation(&self) -> bool {
        self.0
    }

    fn create(&self, _: &CreateRequest, _: ControlSender) -> anyhow::Result<CreatedComponent> {
        panic!("capability discovery must not construct a query")
    }
}

#[test]
fn factory_query_opt_in_changes_only_v4_not_frozen_metadata() -> anyhow::Result<()> {
    let mut manifests = Vec::new();
    for expected in [false, true] {
        let export = ExportedPlugin::new(PluginDefinition::new(
            "query-capability",
            "1",
            vec![Arc::new(QueryFactory(expected))],
            vec![GraphChangeCodec::schema()],
        )?)?;
        let metadata = unsafe { transport::checked_table(export.metadata())? };
        manifests.push(
            unsafe { transport::borrowed_bytes(metadata.manifest, abi::MAX_METADATA_BYTES)? }
                .to_vec(),
        );
        let extension = unsafe { transport::checked_table(export.telemetry_v4())? };
        let mut plugin = abi::PluginHandle::null();
        unsafe { export.entry(&mut plugin)? };
        let table = unsafe { transport::checked_table(plugin.vtable)? };
        let mut factory = abi::FactoryHandle::null();
        unsafe {
            transport::take_status(table.factory.expect("complete")(
                plugin.state,
                0,
                &mut factory,
            ))?;
            let mut actual = u32::MAX;
            transport::take_status(extension.supports_factory.expect("complete")(
                factory.state,
                &mut actual,
            ))?;
            assert_eq!(actual, u32::from(expected));
            transport::checked_table(factory.vtable)?
                .release
                .expect("complete")(factory.state);
            table.release.expect("complete")(plugin.state);
        }
    }
    assert_eq!(
        manifests[0], manifests[1],
        "optional V4 opt-in must not change base metadata bytes"
    );
    Ok(())
}
