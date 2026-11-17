// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0
use crate::api::App;
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::Arc;

fn status(response: &Value) -> &str {
    match response["status"].as_str() {
        Some("completed") => "Completed",
        Some("failed" | "cancelled" | "incomplete") => "Failed",
        Some("queued" | "in_progress") => "Running",
        _ => "Unknown",
    }
}

pub async fn tick(app: &App) -> Result<()> {
    let records = app.runtime.store.read().await;
    for routine in records.routines.values().filter(|r| r.status == "Active") {
        let live = app
            .runtime
            .live
            .lock()
            .await
            .get(&routine.id)
            .map(|live| (live.handle.clone(), live.reaction_id.clone()));
        if let Some((handle, reaction_id)) = live {
            let query_error = app.runtime.core.get_query_status(&handle.query_id).await?
                == drasi_lib::channels::ComponentStatus::Error;
            let reaction_error = app.runtime.core.get_reaction_status(&reaction_id).await?
                == drasi_lib::channels::ComponentStatus::Error;
            if query_error || reaction_error {
                handle.gate.lock().await.enabled = false;
                app.runtime
                    .store
                    .update(|records| {
                        let saved = records
                            .routines
                            .get_mut(&routine.id)
                            .context("Routine disappeared")?;
                        saved.status = "Error".into();
                        saved.error = Some(
                            "Query/reaction ingestion failed. Inspect the error before resuming."
                                .into(),
                        );
                        Ok(())
                    })
                    .await?;
            }
        }
        for source in &routine.sources {
            if app
                .runtime
                .core
                .get_source_status(&source.connection_id)
                .await?
                == drasi_lib::channels::ComponentStatus::Error
            {
                if let Some(handle) = app
                    .runtime
                    .live
                    .lock()
                    .await
                    .get(&routine.id)
                    .map(|r| r.handle.clone())
                {
                    handle.gate.lock().await.enabled = false;
                }
                app.runtime
                    .store
                    .update(|records| {
                        let saved = records
                            .routines
                            .get_mut(&routine.id)
                            .context("Routine disappeared")?;
                        saved.status = "Error".into();
                        saved.error = Some(format!(
                            "Source '{}' stopped with an ingestion error",
                            source.connection_id
                        ));
                        Ok(())
                    })
                    .await?;
            }
        }
    }
    for run in records.runs.values().filter(|r| r.status == "Running") {
        let id = run
            .response
            .as_ref()
            .and_then(|v| v["id"].as_str())
            .context("Running response has no identifier")?;
        match app.foundry.response(&run.agent, id).await {
            Ok(response) => {
                app.runtime
                    .store
                    .update(|r| {
                        let saved = r.runs.get_mut(&run.id).context("Run disappeared")?;
                        saved.status = status(&response).into();
                        saved.response = Some(response);
                        saved.updated_at = chrono::Utc::now().to_rfc3339();
                        Ok(())
                    })
                    .await?
            }
            Err(error) => {
                log::error!("Response observation failed: {error:#}");
                app.runtime
                    .store
                    .update(|r| {
                        r.runs.get_mut(&run.id).context("Run disappeared")?.error =
                            Some(format!("Observation failed; response ID retained: {error}"));
                        Ok(())
                    })
                    .await?;
            }
        }
    }
    if records
        .runs
        .values()
        .filter(|run| run.status == "Running")
        .count()
        >= 2
    {
        return Ok(());
    }
    let mut queued: Vec<_> = records
        .runs
        .values()
        .filter(|r| r.status == "Queued")
        .collect();
    queued.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    for run in queued {
        if records
            .runs
            .values()
            .any(|r| r.routine_id == run.routine_id && r.status == "Running")
        {
            continue;
        }
        let handle = app
            .runtime
            .live
            .lock()
            .await
            .get(&run.routine_id)
            .map(|live| live.handle.clone());
        let Some(handle) = handle else { continue };
        let gate = handle.gate.lock().await;
        if !gate.enabled {
            continue;
        }
        let claimed = app
            .runtime
            .store
            .update(|records| {
                let saved = records.runs.get_mut(&run.id).context("Run disappeared")?;
                if saved.status != "Queued" {
                    return Ok(false);
                }
                saved.status = "Submitting".into();
                saved.updated_at = chrono::Utc::now().to_rfc3339();
                Ok(true)
            })
            .await?;
        if !claimed {
            continue;
        }
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(90),
            app.foundry.invoke(&run.agent, &run.input),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(crate::foundry::SubmissionFailure {
                uncertain: true,
                attempts: 1,
                message: "Submission deadline exceeded; reconcile before resubmitting".into(),
            }),
        };
        app.runtime
            .store
            .update(|records| {
                let saved = records.runs.get_mut(&run.id).context("Run disappeared")?;
                match result {
                    Ok((response, attempts)) => {
                        saved.status = status(&response).into();
                        saved.response = Some(response);
                        saved.error = None;
                        saved.attempts = attempts;
                    }
                    Err(error) => {
                        saved.status = if error.uncertain {
                            "Unknown".into()
                        } else {
                            "Failed".into()
                        };
                        saved.error = Some(error.to_string());
                        saved.attempts = error.attempts;
                    }
                }
                saved.updated_at = chrono::Utc::now().to_rfc3339();
                Ok(())
            })
            .await?;
        drop(gate);
        break;
    }
    Ok(())
}

pub async fn run(app: Arc<App>, mut shutdown: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
    loop {
        tokio::select! {
            _=&mut shutdown=>return Ok(()),
            _=tokio::time::sleep(std::time::Duration::from_millis(250))=> {
                tick(&app).await?;
            }
        }
    }
}
