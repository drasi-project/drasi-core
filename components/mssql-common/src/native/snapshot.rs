// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    catalog::SqlClient, element_size, engine::element, quoted, table_name, Catalog, Cursor,
    NativeConfig, Work,
};
use crate::{Lsn, MsSqlConnection};
use anyhow::{ensure, Context, Result};
use drasi_core_types::models::SourceChange;
use futures_util::TryStreamExt;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch, Mutex, OwnedSemaphorePermit, Semaphore};

struct BufferedRow {
    change: SourceChange,
    _bytes: OwnedSemaphorePermit,
}
struct Sender {
    rows: mpsc::Sender<BufferedRow>,
    bytes: Arc<Semaphore>,
    max_bytes: usize,
    cancel: watch::Receiver<bool>,
}
impl Sender {
    async fn send(&self, change: SourceChange) -> Result<()> {
        let SourceChange::Insert { element } = &change else {
            anyhow::bail!("snapshot must emit inserts")
        };
        let size = element_size(element)?;
        ensure!(
            size <= self.max_bytes,
            "one SQL Server snapshot record exceeds max_bytes; no truncation performed"
        );
        let mut cancel = self.cancel.clone();
        let permit = tokio::select! {
            biased;
            _=cancel.wait_for(|stopped|*stopped)=>anyhow::bail!("SQL Server snapshot cancelled"),
            permit=self.bytes.clone().acquire_many_owned(u32::try_from(size)?)=>permit?,
        };
        tokio::select! {
            biased;
            _=cancel.wait_for(|stopped|*stopped)=>anyhow::bail!("SQL Server snapshot cancelled"),
            result=self.rows.send(BufferedRow {change,_bytes:permit})=>result.context("SQL Server snapshot reader closed"),
        }
    }
}
#[derive(Default)]
struct Completion {
    boundary: Option<Cursor>,
    operation_error: Option<String>,
    cleanup_error: Option<String>,
}
struct Worker {
    work: Work<Completion>,
    completed: Option<Arc<Completion>>,
}
pub struct SnapshotRun {
    worker: Mutex<Worker>,
    cancel: watch::Sender<bool>,
    drained: AtomicBool,
    timeout: Duration,
}
impl SnapshotRun {
    fn spawn<F, Fut>(
        rows: usize,
        bytes: usize,
        timeout: Duration,
        job: F,
    ) -> Result<(Arc<Self>, SnapshotRows)>
    where
        F: FnOnce(Sender, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = Completion> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(rows.min(64));
        let (cancel, stop) = watch::channel(false);
        let sender = Sender {
            rows: tx,
            bytes: Arc::new(Semaphore::new(bytes)),
            max_bytes: bytes,
            cancel: stop.clone(),
        };
        let mut work = Work::default();
        work.start(async move {
            let result = job(sender, stop).await;
            if let Some(error) = &result.cleanup_error {
                log::error!("SQL Server snapshot cleanup not confirmed: {error}");
            }
            Ok(result)
        })?;
        let run = Arc::new(Self {
            worker: Mutex::new(Worker {
                work,
                completed: None,
            }),
            cancel,
            drained: AtomicBool::new(false),
            timeout,
        });
        Ok((
            run.clone(),
            SnapshotRows {
                run,
                rows: rx,
                ended: false,
            },
        ))
    }
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }
    async fn result(&self, timeout: Duration) -> Result<Arc<Completion>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut worker = tokio::time::timeout_at(deadline, self.worker.lock())
            .await
            .context("SQL Server snapshot ownership wait timed out; cleanup remains retained")?;
        if let Some(result) = &worker.completed {
            return Ok(result.clone());
        }
        let completion = match worker
            .work
            .wait(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await
        {
            Ok(completion) => completion,
            Err(error) if worker.work.pending() => return Err(error),
            Err(error) => Completion {
                cleanup_error: Some(format!(
                    "snapshot worker failed without cleanup confirmation: {error:#}"
                )),
                ..Default::default()
            },
        };
        let result = Arc::new(completion);
        worker.completed = Some(result.clone());
        Ok(result)
    }
    pub async fn complete(&self) -> Result<Option<Cursor>> {
        ensure!(
            self.drained.load(Ordering::Acquire) && !*self.cancel.borrow(),
            "SQL Server snapshot has not been completely consumed or was retired"
        );
        let result = self.result(self.timeout).await?;
        ensure_success(&result)?;
        Ok(result.boundary.clone())
    }
    pub async fn stop(&self, timeout: Duration) -> Result<()> {
        self.cancel();
        let result = self.result(timeout).await?;
        if let Some(error) = &result.cleanup_error {
            anyhow::bail!("{error}");
        }
        // A data error is not a successful snapshot; cleanup can nevertheless
        // succeed after the owned worker confirms rollback/marker retirement.
        Ok(())
    }
}
impl Drop for SnapshotRun {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}
fn ensure_success(result: &Completion) -> Result<()> {
    if let Some(error) = &result.cleanup_error {
        anyhow::bail!("{error}");
    }
    if let Some(error) = &result.operation_error {
        anyhow::bail!("{error}");
    }
    Ok(())
}

pub struct SnapshotRows {
    run: Arc<SnapshotRun>,
    rows: mpsc::Receiver<BufferedRow>,
    ended: bool,
}
impl SnapshotRows {
    pub async fn next(&mut self) -> Result<Option<SourceChange>> {
        if self.ended {
            return Ok(None);
        }
        if *self.run.cancel.borrow() {
            self.ended = true;
            anyhow::bail!("SQL Server snapshot stream is retired");
        }
        let mut cancel = self.run.cancel.subscribe();
        let received = tokio::select! {
            biased;
            _=cancel.wait_for(|stopped|*stopped)=>{
                self.ended=true;
                anyhow::bail!("SQL Server snapshot stream is retired");
            }
            next=tokio::time::timeout(self.run.timeout,self.rows.recv())=>next,
        };
        let next = match received {
            Ok(next) => next,
            Err(error) => {
                self.run.cancel();
                self.rows.close();
                self.ended = true;
                return Err(anyhow::Error::from(error).context(
                    "SQL Server snapshot read timed out; owned cleanup is still required",
                ));
            }
        };
        if *self.run.cancel.borrow() {
            self.ended = true;
            anyhow::bail!("SQL Server snapshot stream is retired");
        }
        match next {
            Some(row) => Ok(Some(row.change)),
            None => {
                let result = match self.run.result(self.run.timeout).await {
                    Ok(result) => result,
                    Err(error) => {
                        self.run.cancel();
                        self.ended = true;
                        return Err(error);
                    }
                };
                if let Err(error) = ensure_success(&result) {
                    self.run.cancel();
                    self.ended = true;
                    return Err(error);
                }
                ensure!(
                    !*self.run.cancel.borrow(),
                    "SQL Server snapshot stream is retired"
                );
                self.ended = true;
                self.run.drained.store(true, Ordering::Release);
                Ok(None)
            }
        }
    }
}
impl Drop for SnapshotRows {
    fn drop(&mut self) {
        if !self.run.drained.load(Ordering::Acquire) {
            self.run.cancel();
        }
    }
}

pub fn start_snapshot(
    config: NativeConfig,
    source: String,
    labels: Vec<String>,
    coordinated: bool,
) -> Result<(Arc<SnapshotRun>, SnapshotRows)> {
    config.validate()?;
    ensure!(
        coordinated == config.barrier_table.is_some(),
        "SQL Server snapshot mode and marker configuration disagree"
    );
    SnapshotRun::spawn(
        config.max_rows,
        config.max_bytes,
        config.timeout(),
        move |sender, cancel| {
            database_snapshot(config, source, labels, coordinated, sender, cancel)
        },
    )
}
fn check_cancel(cancel: &watch::Receiver<bool>) -> Result<()> {
    ensure!(!*cancel.borrow(), "SQL Server snapshot cancelled");
    Ok(())
}

async fn database_snapshot(
    config: NativeConfig,
    source: String,
    labels: Vec<String>,
    coordinated: bool,
    sender: Sender,
    cancel: watch::Receiver<bool>,
) -> Completion {
    let mut connection = match MsSqlConnection::connect(&config.connection).await {
        Ok(connection) => connection,
        Err(error) => {
            return Completion {
                operation_error: Some(format!("{error:#}")),
                ..Default::default()
            }
        }
    };
    let mut marker = if coordinated {
        match MsSqlConnection::connect(&config.connection).await {
            Ok(connection) => Some(connection),
            Err(error) => {
                return Completion {
                    operation_error: Some(format!("{error:#}")),
                    ..Default::default()
                }
            }
        }
    } else {
        None
    };
    let token = uuid::Uuid::new_v4();
    let mut marker_table = None;
    let mut marker_attempted = false;
    let client = connection.client_mut();
    let result=async {
        check_cancel(&cancel)?;
        let catalog=if coordinated {Some(Catalog::read(client,&config).await?)} else {None};
        let isolation=if coordinated {"SERIALIZABLE"} else {"SNAPSHOT"};
        client.simple_query(format!("SET LOCK_TIMEOUT {}; SET TRANSACTION ISOLATION LEVEL {isolation}; BEGIN TRANSACTION",
            i32::try_from(config.operation_timeout_ms)?)).await?.into_results().await?;
        let catalog=match catalog {Some(catalog)=>catalog,None=>Catalog::read_snapshot(client,&config).await?};
        let boundary=if let Some(marker)=marker.as_mut() {
            for table in &catalog.tables {
                check_cancel(&cancel)?;
                client.simple_query(format!("SELECT TOP (1) 1 AS locked FROM {} WITH (TABLOCK,HOLDLOCK)",table.sql()))
                    .await?.into_results().await?;
            }
            let (schema,name)=table_name(config.barrier_table.as_deref().context("CDC barrier missing")?)?;
            marker_table=Some(format!("{}.{}",quoted(&schema),quoted(&name)));
            let marker=marker.client_mut();
            let captures=marker.query(
                "SELECT ct.capture_instance FROM cdc.change_tables ct JOIN sys.tables t ON t.object_id=ct.source_object_id \
                 JOIN sys.schemas s ON s.schema_id=t.schema_id WHERE s.name=@P1 AND t.name=@P2",&[&schema,&name])
                .await?.into_first_result().await?;
            ensure!(captures.len()==1,"SQL Server barrier must have exactly one CDC capture instance");
            let capture=captures[0].try_get::<&str,_>(0)?.context("missing barrier capture")?.to_owned();
            check_cancel(&cancel)?;
            marker_attempted=true;
            marker.execute(format!("INSERT INTO {} (token) VALUES (@P1)",marker_table.as_ref().context("marker table")?),&[&token]).await?;
            let deadline=tokio::time::Instant::now()+config.timeout();
            let boundary=loop {
                check_cancel(&cancel)?;
                let rows=marker.query(format!("SELECT __$start_lsn FROM cdc.{} WHERE token=@P1 AND __$operation=2",
                    quoted(&format!("{capture}_CT"))),&[&token]).await?.into_first_result().await?;
                if let Some(row)=rows.first() {
                    break *Lsn::from_bytes(row.try_get::<&[u8],_>(0)?.context("barrier LSN missing")?)?.as_bytes();
                }
                ensure!(tokio::time::Instant::now()<deadline,"SQL Server CDC barrier capture timed out");
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            Some(Cursor::boundary(catalog.binding,boundary))
        } else {None};
        stream_tables(client,&config,&source,&labels,&catalog,&sender,&cancel).await?;
        let verified=if coordinated {Catalog::read(client,&config).await?} else {Catalog::read_snapshot(client,&config).await?};
        ensure!(verified.binding==catalog.binding,"SQL Server schema changed during snapshot");
        if let Some(boundary)=&boundary {
            for table in &catalog.tables {
                let min=super::engine::lsn(client,"SELECT sys.fn_cdc_get_min_lsn(@P1)",&[&table.capture]).await?;
                ensure!(min!=[0;10] && min<=boundary.lsn,"SQL Server snapshot CDC boundary expired before completion");
            }
        }
        check_cancel(&cancel)?;
        client.simple_query("COMMIT TRANSACTION").await?.into_results().await?;
        Ok::<_,anyhow::Error>(boundary)
    }.await;
    // No SQL future is abandoned on cancellation. An in-flight FETCH contains
    // at most the configured bounded batch; cleanup drains it before rollback.
    let rollback = async {
        client
            .simple_query(
                "IF CURSOR_STATUS('global','drasi_native_snapshot')>=0 CLOSE drasi_native_snapshot;
            IF CURSOR_STATUS('global','drasi_native_snapshot')>=-1 DEALLOCATE drasi_native_snapshot;
            IF @@TRANCOUNT>0 ROLLBACK TRANSACTION",
            )
            .await?
            .into_results()
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = async {
        if marker_attempted {
            marker
                .as_mut()
                .context("owned marker connection lost")?
                .client_mut()
                .execute(
                    format!(
                        "DELETE FROM {} WHERE token=@P1",
                        marker_table.as_ref().context("owned marker table lost")?
                    ),
                    &[&token],
                )
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup_error = if rollback.is_err() || cleanup.is_err() {
        Some(format!("SQL Server snapshot cleanup not confirmed for token {token}: rollback={rollback:?}, marker={cleanup:?}"))
    } else {
        None
    };
    match result {
        Ok(boundary) => Completion {
            boundary,
            operation_error: None,
            cleanup_error,
        },
        Err(error) => Completion {
            boundary: None,
            operation_error: Some(format!("{error:#}")),
            cleanup_error,
        },
    }
}

async fn stream_tables(
    client: &mut SqlClient,
    config: &NativeConfig,
    source: &str,
    labels: &[String],
    catalog: &Catalog,
    sender: &Sender,
    cancel: &watch::Receiver<bool>,
) -> Result<()> {
    let batch = config.max_rows.min(64);
    let fetch = "FETCH NEXT FROM drasi_native_snapshot;".repeat(batch);
    let timestamp = chrono::Utc::now().timestamp_millis() as u64;
    for table in &catalog.tables {
        if !labels.is_empty()
            && !labels.iter().any(|label| {
                label.eq_ignore_ascii_case(&table.name)
                    || label.eq_ignore_ascii_case(&table.canonical())
            })
        {
            continue;
        }
        check_cancel(cancel)?;
        // GLOBAL is scoped to this dedicated SQL session, so it survives TDS
        // batches. It is never shared with application or another provider work.
        client
            .simple_query(format!(
                "DECLARE drasi_native_snapshot CURSOR GLOBAL FAST_FORWARD FOR SELECT {} FROM {};
            OPEN drasi_native_snapshot;",
                table.selection(),
                table.sql()
            ))
            .await?
            .into_results()
            .await?;
        loop {
            check_cancel(cancel)?;
            let mut rows = client.simple_query(&fetch).await?.into_row_stream();
            let mut count = 0;
            while let Some(row) = rows.try_next().await? {
                check_cancel(cancel)?;
                let element = element(source, table, &row, timestamp)?;
                drop(row);
                sender.send(SourceChange::Insert { element }).await?;
                count += 1;
            }
            if count < batch {
                break;
            }
        }
        client
            .simple_query("CLOSE drasi_native_snapshot; DEALLOCATE drasi_native_snapshot;")
            .await?
            .into_results()
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
    };
    fn row() -> SourceChange {
        let mut properties = ElementPropertyMap::new();
        properties.insert("text", ElementValue::String(Arc::from("Unicode 雪")));
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("s", "1"),
                    labels: Arc::from([Arc::from("Items")]),
                    effective_from: 1,
                },
                properties,
            },
        }
    }
    async fn bounded() -> Result<()> {
        let SourceChange::Insert { element } = row() else {
            panic!("row")
        };
        let size = element_size(&element)?;
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = sent.clone();
        let (run, mut rows) = SnapshotRun::spawn(
            1,
            size,
            Duration::from_secs(2),
            move |sender, _| async move {
                for _ in 0..1000 {
                    if let Err(error) = sender.send(row()).await {
                        return Completion {
                            operation_error: Some(error.to_string()),
                            ..Default::default()
                        };
                    }
                    count.fetch_add(1, Ordering::Release);
                }
                Completion {
                    boundary: Some(Cursor::boundary([1; 32], [7; 10])),
                    ..Default::default()
                }
            },
        )?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(sent.load(Ordering::Acquire), 1);
        assert!(run.complete().await.is_err());
        let mut count = 0;
        while rows.next().await?.is_some() {
            count += 1;
        }
        assert_eq!(count, 1000);
        assert_eq!(run.complete().await?.expect("cut").lsn, [7; 10]);
        run.stop(Duration::from_secs(1)).await?;
        Ok(())
    }
    #[tokio::test]
    async fn large_stream_with_one_row_and_exact_byte_credit_current_thread() -> Result<()> {
        bounded().await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_stream_with_one_row_and_exact_byte_credit_multi_thread() -> Result<()> {
        bounded().await
    }
    #[tokio::test]
    async fn oversized_record_fails_without_a_cut_and_cleanup_failure_never_becomes_success(
    ) -> Result<()> {
        let SourceChange::Insert { element } = row() else {
            panic!("row")
        };
        let bytes = element_size(&element)? - 1;
        let (run, mut rows) =
            SnapshotRun::spawn(1, bytes, Duration::from_secs(1), |sender, _| async move {
                let error = sender.send(row()).await.expect_err("oversized row");
                Completion {
                    operation_error: Some(error.to_string()),
                    cleanup_error: Some("rollback not confirmed".into()),
                    boundary: None,
                }
            })?;
        assert!(rows.next().await.is_err());
        assert!(run.complete().await.is_err());
        assert!(run.stop(Duration::from_secs(1)).await.is_err());
        assert!(run.stop(Duration::from_secs(1)).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_retains_worker_until_cleanup_and_does_not_deadlock_on_full_queue(
    ) -> Result<()> {
        let (release, cleanup) = tokio::sync::oneshot::channel::<()>();
        let (run, rows) =
            SnapshotRun::spawn(1, 4096, Duration::from_secs(1), |sender, _| async move {
                sender.send(row()).await.expect("first");
                let failed = sender.send(row()).await.is_err();
                cleanup.await.expect("cleanup release");
                Completion {
                    operation_error: failed.then(|| "cancelled".into()),
                    ..Default::default()
                }
            })?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(rows);
        assert!(run.stop(Duration::from_millis(1)).await.is_err());
        release.send(()).expect("release");
        run.stop(Duration::from_secs(1)).await?;
        assert!(run.complete().await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn cancelling_an_eof_wait_never_publishes_a_cut_before_cleanup() -> Result<()> {
        let (release, cleanup) = tokio::sync::oneshot::channel::<()>();
        let (run, mut rows) =
            SnapshotRun::spawn(1, 4096, Duration::from_secs(2), |sender, _| async move {
                drop(sender);
                cleanup.await.expect("cleanup gate");
                Completion {
                    boundary: Some(Cursor::boundary([1; 32], [8; 10])),
                    ..Default::default()
                }
            })?;
        {
            let mut next = Box::pin(rows.next());
            tokio::select! {
                result=&mut next=>panic!("EOF before cleanup: {result:?}"),
                _=tokio::time::sleep(Duration::from_millis(5))=>{}
            }
        }
        assert!(run.complete().await.is_err());
        release.send(()).expect("release");
        assert!(rows.next().await?.is_none());
        assert!(run.complete().await?.is_some());
        run.stop(Duration::from_secs(1)).await?;
        Ok(())
    }
    #[tokio::test]
    async fn stop_timeout_includes_waiting_for_an_owned_eof_reader() -> Result<()> {
        let (release, cleanup) = tokio::sync::oneshot::channel::<()>();
        let (run, mut rows) =
            SnapshotRun::spawn(1, 4096, Duration::from_secs(2), |sender, _| async move {
                drop(sender);
                cleanup.await.expect("cleanup gate");
                Completion::default()
            })?;
        let mut pending = Box::pin(rows.next());
        tokio::select! {
            result=&mut pending=>panic!("unexpected completion: {result:?}"),
            _=tokio::time::sleep(Duration::from_millis(5))=>{}
        }
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            run.stop(Duration::from_millis(1))
        )
        .await?
        .is_err());
        release.send(()).expect("release");
        assert!(pending.await.is_err());
        run.stop(Duration::from_secs(1)).await?;
        assert!(run.complete().await.is_err());
        Ok(())
    }
}
