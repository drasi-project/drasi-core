// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
fn main() -> Result<()> {
    foundry_routines_demo::local_config::load()?;
    env_logger::init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    use foundry_routines_demo::{
        api::App, delivery, fixtures, foundry::Foundry, runtime::Runtime, store::Store,
    };
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.iter().all(|a| a == "--offline" || a == "--fixtures"),
        "Supported flags: --offline --fixtures"
    );
    let offline = args.iter().any(|a| a == "--offline");
    let fixture_mode = args.iter().any(|a| a == "--fixtures");
    let dir = std::env::var_os("FOUNDRY_DEMO_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".demo"));
    std::fs::create_dir_all(&dir)?;
    let provider = std::sync::Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(
        std::path::Path::new(&dir).join("state.redb"),
    )?);
    let store = Store::open(provider).await?;
    if fixture_mode {
        fixtures::load(&store, offline).await?;
    }
    let runtime = Runtime::new(store).await?;
    let app = App::new(runtime.clone(), Foundry::new(offline)?, fixture_mode, 8780);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let mut worker = tokio::spawn(delivery::run(app.clone(), shutdown_rx));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8780").await?;
    println!("Foundry integration concept: http://127.0.0.1:8780");
    if offline {
        println!("OFFLINE TEST TRANSPORT: real database CDC, simulated agent responses");
    } else if app.foundry.project.is_none() {
        println!(
            "Set FOUNDRY_PROJECT_ENDPOINT in examples/lib/foundry-routines/.env, then restart. Authentication uses your default Azure developer credentials; no API key or token is needed."
        );
    }
    let mut server_receive = app.shutdown.subscribe();
    let shutdown_app = app.clone();
    let server =
        axum::serve(listener, App::router(app.clone())).with_graceful_shutdown(async move {
            tokio::select! {
                result=tokio::signal::ctrl_c()=>{
                    if let Err(e)=result {log::error!("Shutdown signal error: {e}");}
                    shutdown_app.shutdown.send_replace(true);
                },
                _=server_receive.changed()=>{},
            }
        });
    let server = async { server.await };
    tokio::pin!(server);
    let server_result = tokio::select! {
        result=&mut server=>result,
        result=&mut worker=>{
            app.shutdown.send_replace(true);
            server.await?;
            runtime.core.stop().await?;
            result??;
            anyhow::bail!("Dispatcher exited unexpectedly");
        }
    };
    let _ = shutdown_tx.send(());
    match tokio::time::timeout(std::time::Duration::from_secs(35), &mut worker).await {
        Ok(result) => result??,
        Err(_) => {
            worker.abort();
            match worker.await {
                Ok(result) => result?,
                Err(error) if error.is_cancelled() => {}
                Err(error) => return Err(error.into()),
            }
            runtime.core.stop().await?;
            anyhow::bail!(
                "Dispatcher shutdown timed out; inspect Submitting records before restarting"
            );
        }
    }
    runtime.core.stop().await?;
    server_result?;
    Ok(())
}
