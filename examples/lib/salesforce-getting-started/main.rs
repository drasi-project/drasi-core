// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::ResultDiff;
use drasi_lib::indexes::StorageBackendRef;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::Subscription;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_salesforce::SalesforceSource;
use serde_json::json;
use support::{Args, DemoQuery};

const SOURCE: &str = "salesforce-source";

const ACCOUNTS_QUERY: &str = concat!(
    "MATCH (a:Account) ",
    "RETURN a.Id AS id, a.Name AS name, a.Phone AS phone, a.Description AS description"
);
const COUNT_QUERY: &str = "MATCH (a:Account) RETURN count(a) AS count";
const CONTACTS_QUERY: &str = concat!(
    "MATCH (c:Contact) ",
    "RETURN c.Id AS id, c.Name AS name, c.Email AS email, c.AccountId AS accountId"
);
const JOIN_QUERY: &str = concat!(
    "MATCH (c:Contact)-[:Account]->(a:Account) ",
    "RETURN c.Id AS contactId, c.Name AS contact, a.Id AS accountId, a.Name AS account"
);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,drasi_source_salesforce=info"),
    )
    .format_timestamp_secs()
    .init();
    let args = Args::parse();
    if args.local {
        return support::offline::run(args.diagnose).await;
    }

    // The helper handles Salesforce CLI login and keeps credentials out of output.
    let connection = support::connect(&args).await?;
    if args.schema || args.diagnose {
        return support::inspect(&connection, args.schema).await;
    }
    support::prepare_run(&connection)?;
    let source = SalesforceSource::builder(SOURCE)
        .with_config(connection.config)
        .with_identity_provider(connection.identity)
        .build()?;
    let (drasi, mut changes) =
        build_runtime(source, &connection.query_state, &connection.queries).await?;

    let outcome = tokio::select! {
        stopped = tokio::signal::ctrl_c() => stopped.context("Cannot listen for Ctrl+C"),
        result = watch(&drasi, &mut changes, &connection.queries) => result,
    };
    // Always release the source and query databases, including after an error.
    let stopped = drasi.shutdown().await;
    if let Err(error) = &stopped {
        eprintln!("Failed to stop Salesforce runtime cleanly: {error:#}");
    }
    outcome?;
    stopped?;
    println!("STOPPED: source and query state saved");
    Ok(())
}

async fn build_runtime(
    source: SalesforceSource,
    state: &Path,
    queries: &[DemoQuery],
) -> Result<(DrasiLib, Subscription)> {
    let mut reaction = ApplicationReaction::builder("console");
    for query in queries {
        reaction = reaction.with_query(&query.id);
    }
    let (reaction, handle) = reaction.build();
    let changes = handle.subscribe_with_options(Default::default()).await?;

    let mut builder = DrasiLib::builder()
        .with_id("salesforce-getting-started")
        .with_source(source)
        .with_reaction(reaction)
        .with_index_provider(
            "persistent",
            Arc::new(RocksDbIndexProvider::new(state, false, false)),
        );
    for query in queries {
        builder = builder.with_query(
            Query::cypher(&query.id)
                .query(&query.text)
                .from_source(SOURCE)
                .enable_bootstrap(true)
                .with_storage_backend(StorageBackendRef::Named("persistent".into()))
                .build(),
        );
    }
    Ok((builder.build().await?, changes))
}

async fn watch(drasi: &DrasiLib, changes: &mut Subscription, queries: &[DemoQuery]) -> Result<()> {
    drasi.start().await?;
    support::wait_until_ready(drasi, queries).await?;
    println!("READY: watching the real Salesforce org");
    let mut previous = support::print_state(drasi, queries, None).await?;
    let mut health = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            result = changes.recv() => {
                let result = result.context("ApplicationReaction subscription closed")?;
                for change in result.results {
                    match change {
                        ResultDiff::Add { data, .. } => println!("ADD {} {data}", result.query_id),
                        ResultDiff::Update { before, after, .. } => {
                            println!("UPDATE {} {before} -> {after}", result.query_id);
                        }
                        ResultDiff::Delete { data, .. } => println!("DELETE {} {data}", result.query_id),
                        ResultDiff::Aggregation { before, after, .. } => {
                            println!("AGGREGATE {} {} -> {after}", result.query_id, json!(before));
                        }
                        ResultDiff::Noop => {}
                    }
                }
            }
            _ = health.tick() => {
                ensure!(support::queries_ready(drasi, queries).await?, "A query stopped while watching Salesforce");
                previous = support::print_state(drasi, queries, Some(&previous)).await?;
            }
        }
    }
}
