// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_lib::{DrasiLib, Query};

fn query() -> drasi_lib::config::QueryConfig {
    Query::cypher("query")
        .query("MATCH (n:Item) RETURN n.value AS value")
        .enable_bootstrap(false)
        .build()
}

#[tokio::test]
async fn builder_queries_require_explicit_factory_registration() {
    let result = DrasiLib::builder().with_query(query()).build().await;
    let error = result
        .err()
        .expect("query builder must not select a built-in implementation");
    assert!(error
        .to_string()
        .contains("explicitly registered query factory"));
}

#[tokio::test]
async fn empty_instance_and_source_only_pipeline_do_not_require_a_query_factory(
) -> anyhow::Result<()> {
    let core = DrasiLib::builder().build().await?;
    let run = async {
        core.computation_pipeline()?.build()?;
        let error = core
            .add_query(query())
            .await
            .expect_err("late query creation must not select a built-in implementation");
        anyhow::ensure!(
            error
                .to_string()
                .contains("explicitly registered query factory"),
            "missing query registration returned an unrelated failure: {error}"
        );
        anyhow::ensure!(
            core.list_queries().await?.is_empty(),
            "missing registration installed a partial query"
        );
        let result = core.computation_pipeline()?.query(query()).build();
        anyhow::ensure!(
            result.err().is_some_and(|error| error
                .to_string()
                .contains("explicitly registered query factory")),
            "pipeline query creation did not require explicit registration"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let stopped = core.shutdown().await;
    run?;
    stopped?;
    Ok(())
}
