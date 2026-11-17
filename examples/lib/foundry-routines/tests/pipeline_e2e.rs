// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0
use anyhow::{ensure, Result};
use foundry_routines_demo::{fixtures, runtime::Runtime, store::Store};
use mysql_async::prelude::Queryable;
use std::{sync::Arc, time::Duration};

async fn stock(available: i32) -> Result<()> {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(13306)
        .user(Some("root"))
        .pass(Some("local-fixture-admin"))
        .db_name(Some("inventory"));
    let mut conn = mysql_async::Conn::new(opts).await?;
    conn.exec_drop(
        "UPDATE inventory SET available = ? WHERE sku = 'SKU-A'",
        (available,),
    )
    .await?;
    conn.disconnect().await?;
    Ok(())
}

async fn order(sql: &str) -> Result<()> {
    let (client, driver) = tokio_postgres::connect(
        "host=127.0.0.1 port=15432 user=postgres password=local-fixture-admin dbname=orders",
        tokio_postgres::NoTls,
    )
    .await?;
    let driver = tokio::spawn(driver);
    let result = client.batch_execute(sql).await;
    drop(client);
    driver.await??;
    result?;
    Ok(())
}

async fn wait_runs(store: &Store, count: usize) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if store.read().await.runs.len() == count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned Compose database fixtures; run with --ignored"]
async fn databases_join_and_future_only_pause_boundaries() -> Result<()> {
    std::env::set_var("FOUNDRY_DEMO_POSTGRES_PASSWORD", "local-fixture-only");
    std::env::set_var("FOUNDRY_DEMO_MYSQL_PASSWORD", "local-fixture-only");
    stock(3).await?;
    order("UPDATE orders SET quantity=3 WHERE order_id='order-1002'").await?;
    let dir = tempfile::tempdir()?;
    let provider = Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(
        dir.path().join("state.redb"),
    )?);
    let store = Store::open(provider).await?;
    fixtures::load(&store, true).await?;
    let routine = store.read().await.routines["order-risk"].clone();
    let runtime = Runtime::new(store.clone()).await?;
    let handle = runtime
        .start(&routine, foundry_routines_demo::runtime::StartMode::Active)
        .await?;
    let snapshot = handle.snapshot().await?;
    ensure!(
        !snapshot["rows"].as_array().unwrap().is_empty(),
        "Existing risk must appear in preview"
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    ensure!(
        store.read().await.runs.is_empty(),
        "Bootstrap must not invoke"
    );
    stock(10).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    stock(2).await?;
    wait_runs(&store, 1).await?;
    {
        handle.gate.lock().await.enabled = false;
    }
    stock(10).await?;
    stock(1).await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let snapshot = handle.snapshot().await?;
    {
        let mut gate = handle.gate.lock().await;
        gate.cutoff = snapshot["sequence"].as_str().unwrap().parse()?;
        gate.enabled = true;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    ensure!(
        store.read().await.runs.len() == 1,
        "Pause backlog must not invoke"
    );
    stock(10).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    stock(2).await?;
    wait_runs(&store, 2).await?;
    order("UPDATE orders SET quantity=12 WHERE order_id='order-1002'").await?;
    wait_runs(&store, 3).await?;
    order("UPDATE orders SET notes=notes || ' unrelated' WHERE order_id='order-1001'").await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    ensure!(
        store.read().await.runs.len() == 3,
        "Unrelated updates must not invoke"
    );
    let app = foundry_routines_demo::api::App::new(
        runtime.clone(),
        foundry_routines_demo::foundry::Foundry::new(true)?,
        true,
        8780,
    );
    for _ in 0..3 {
        foundry_routines_demo::delivery::tick(&app).await?;
    }
    ensure!(
        store
            .read()
            .await
            .runs
            .values()
            .all(|r| r.status == "Completed"
                && r.response
                    .as_ref()
                    .is_some_and(|v| v["id"].as_str().is_some())),
        "The dispatcher must record returned agent responses"
    );
    let mut timings = Vec::new();
    for index in 0..20 {
        stock(10).await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = handle.snapshot().await.unwrap();
                if !snapshot["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["order_id"] == "order-1001")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        stock(2).await?;
        let committed = std::time::Instant::now();
        wait_runs(&store, index + 4).await?;
        timings.push(committed.elapsed());
    }
    timings.sort();
    let p95 = timings[18];
    println!(
        "Database acknowledgement -> durable admission: median={:?}, p95={p95:?}",
        timings[10]
    );
    ensure!(
        p95 <= Duration::from_secs(2),
        "Prewarmed admission p95 exceeds the demo's two-second goal"
    );
    let old_reaction = runtime.live.lock().await[&routine.id].reaction_id.clone();
    runtime.core.stop_reaction(&old_reaction).await?;
    store
        .update(|records| {
            records.routines.get_mut(&routine.id).unwrap().status = "Error".into();
            Ok(())
        })
        .await?;
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/routines/{}/resume", routine.id))
        .header("host", "127.0.0.1:8780")
        .body(axum::body::Body::empty())?;
    ensure!(foundry_routines_demo::api::App::router(app)
        .oneshot(request)
        .await?
        .status()
        .is_success());
    let replacement = runtime.live.lock().await[&routine.id].reaction_id.clone();
    ensure!(
        replacement != old_reaction
            && runtime.core.get_reaction_status(&replacement).await?
                == drasi_lib::channels::ComponentStatus::Running,
        "Resume must rebuild a failed reaction rather than falsely reporting Active"
    );
    ensure!(
        store.read().await.runs.len() == 23,
        "Error recovery must not invoke bootstrap matches"
    );
    runtime.remove(&routine.id).await?;
    runtime.core.stop().await?;
    stock(10).await?;
    order("UPDATE orders SET quantity=3 WHERE order_id='order-1002'").await?;
    Ok(())
}
