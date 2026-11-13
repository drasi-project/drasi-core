// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{Arc, OnceLock};

use drasi_lib::computation::v1::ComponentFactory;

pub fn query_factory() -> Arc<dyn ComponentFactory> {
    static FACTORY: OnceLock<Arc<dyn ComponentFactory>> = OnceLock::new();
    FACTORY
        .get_or_init(|| {
            let plugin = drasi_host_sdk::computation::NativePlugin::from_definition(
                drasi_computation_transformer_query::plugin().expect("native query definition"),
            )
            .expect("native query ABI");
            plugin
                .factories()
                .iter()
                .find(|factory| {
                    factory.metadata().implementation.name.as_ref()
                        == "drasi.experimental/indexed-query"
                })
                .expect("native indexed query factory")
                .for_ordinary_queries()
                .expect("native query authoring")
        })
        .clone()
}

pub fn query_builder() -> drasi_lib::DrasiLibBuilder {
    // Integration fixtures deliberately assemble preconstructed components with fixed IDs.
    drasi_lib::DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(query_factory())
}
