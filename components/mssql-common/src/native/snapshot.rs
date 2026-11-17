// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    catalog::{read_catalog, read_snapshot_catalog},
    element, element_size,
    markers::new_marker,
    quoted,
    session::CdcSession,
    sql_session::{properties, TiberiusConnector, TiberiusSession},
    Catalog, Cursor, NativeSettings, Work,
};
use crate::{client::Authentication, Lsn};
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

/// Starts a snapshot of the selected tables on an owned worker.
///
/// `marker_prefix` selects a coordinated snapshot: it requires
/// `barrierTable`, and the marker it writes carries this prefix (see
/// [`super::markers`]) so the paired source's erasing removal can find an
/// orphan. Without it, the snapshot is read-only and returns no CDC cut.
pub fn start_snapshot(
    settings: NativeSettings,
    authentication: Authentication,
    source: String,
    labels: Vec<String>,
    marker_prefix: Option<String>,
) -> Result<(Arc<SnapshotRun>, SnapshotRows)> {
    settings.validate()?;
    ensure!(
        marker_prefix.is_some() == settings.barrier_table.is_some(),
        "SQL Server snapshot mode and barrierTable disagree"
    );
    SnapshotRun::spawn(
        settings.max_rows,
        settings.max_bytes,
        settings.command_timeout(),
        move |sender, cancel| {
            database_snapshot(
                settings,
                authentication,
                source,
                labels,
                marker_prefix,
                sender,
                cancel,
            )
        },
    )
}
fn check_cancel(cancel: &watch::Receiver<bool>) -> Result<()> {
    ensure!(!*cancel.borrow(), "SQL Server snapshot cancelled");
    Ok(())
}

async fn database_snapshot(
    settings: NativeSettings,
    authentication: Authentication,
    source: String,
    labels: Vec<String>,
    marker_prefix: Option<String>,
    sender: Sender,
    cancel: watch::Receiver<bool>,
) -> Completion {
    let failed = |error: anyhow::Error| Completion {
        operation_error: Some(format!("{error:#}")),
        ..Default::default()
    };
    let connector = TiberiusConnector::new(&settings, &authentication);
    let mut session = match connector.client().await {
        Ok(client) => TiberiusSession::new(client),
        Err(error) => return failed(error),
    };
    let coordinated = marker_prefix.is_some();
    let mut marker = if coordinated {
        match connector.client().await {
            Ok(client) => Some(TiberiusSession::new(client)),
            Err(error) => return failed(error),
        }
    } else {
        None
    };
    let token = match marker_prefix.as_deref().map(new_marker).transpose() {
        Ok(token) => token,
        Err(error) => return failed(error),
    };
    let mut marker_table: Option<String> = None;
    let mut marker_attempted = false;
    let result = async {
        check_cancel(&cancel)?;
        ensure_snapshot_isolation(&mut session).await?;
        let catalog = if coordinated {
            Some(read_catalog(&mut session, &settings, false).await?)
        } else {
            None
        };
        let lock_timeout = i32::try_from(settings.command_timeout_ms)?;
        // The coordinated cut: the marker session locks every selected table
        // (waiting for in-flight writers), the reader establishes its
        // SNAPSHOT view while no selected-table write can commit, and the
        // marker's insert commits in the same transaction that releases the
        // locks. The view therefore holds exactly the changes committed
        // before the marker's LSN. Writers wait only for that short window,
        // never for the scan, the CDC capture or downstream backpressure.
        if let (Some(marker), Some(catalog)) = (marker.as_mut(), catalog.as_ref()) {
            marker
                .client_mut()
                .simple_query(format!(
                    "SET LOCK_TIMEOUT {lock_timeout}; \
                     SET TRANSACTION ISOLATION LEVEL READ COMMITTED; BEGIN TRANSACTION"
                ))
                .await?
                .into_results()
                .await?;
            for table in &catalog.tables {
                check_cancel(&cancel)?;
                marker
                    .client_mut()
                    .simple_query(format!(
                        "SELECT TOP (1) 1 AS locked FROM {} WITH (TABLOCK, HOLDLOCK)",
                        table.sql()
                    ))
                    .await?
                    .into_results()
                    .await?;
            }
        }
        session
            .client_mut()
            .simple_query(format!(
                "SET LOCK_TIMEOUT {lock_timeout}; \
                 SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRANSACTION"
            ))
            .await?
            .into_results()
            .await?;
        let catalog = match catalog {
            Some(catalog) => catalog,
            None => read_snapshot_catalog(&mut session, &settings).await?,
        };
        // A SNAPSHOT transaction's view starts at its first data access.
        for table in &catalog.tables {
            check_cancel(&cancel)?;
            session
                .client_mut()
                .simple_query(format!("SELECT TOP (1) 1 AS seen FROM {}", table.sql()))
                .await?
                .into_results()
                .await?;
        }
        let boundary = if let (Some(marker), Some(token)) = (marker.as_mut(), token.as_deref()) {
            let barrier = settings.barrier()?.context("barrierTable missing")?;
            marker_table = Some(barrier.sql());
            let captures = marker
                .capture_instances()
                .await?
                .into_iter()
                .filter(|capture| barrier.matches(&capture.schema, &capture.table))
                .collect::<Vec<_>>();
            ensure!(
                captures.len() == 1,
                "the SQL Server barrier table must have exactly one CDC capture instance"
            );
            let change_table = format!("cdc.{}", quoted(&format!("{}_CT", captures[0].capture)));
            check_cancel(&cancel)?;
            marker_attempted = true;
            marker
                .client_mut()
                .execute(
                    format!(
                        "INSERT INTO {} (token) VALUES (CONVERT(uniqueidentifier, @P1))",
                        barrier.sql()
                    ),
                    &[&token],
                )
                .await?;
            marker
                .client_mut()
                .simple_query("COMMIT TRANSACTION")
                .await?
                .into_results()
                .await?;
            let deadline = tokio::time::Instant::now() + settings.command_timeout();
            let boundary = loop {
                check_cancel(&cancel)?;
                let rows = marker
                    .client_mut()
                    .query(
                        format!(
                            "SELECT __$start_lsn FROM {change_table} \
                             WHERE token = CONVERT(uniqueidentifier, @P1) AND __$operation = 2"
                        ),
                        &[&token],
                    )
                    .await?
                    .into_first_result()
                    .await?;
                if let Some(row) = rows.first() {
                    break *Lsn::from_bytes(
                        row.try_get::<&[u8], _>(0)?.context("barrier LSN missing")?,
                    )?
                    .as_bytes();
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "SQL Server CDC barrier capture timed out; check that the capture job runs"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            Some(Cursor::boundary(catalog.binding, boundary))
        } else {
            None
        };
        stream_tables(
            &mut session,
            &settings,
            &source,
            &labels,
            &catalog,
            &sender,
            &cancel,
        )
        .await?;
        let verified = if coordinated {
            read_catalog(&mut session, &settings, false).await?
        } else {
            read_snapshot_catalog(&mut session, &settings).await?
        };
        ensure!(
            verified.binding == catalog.binding,
            "SQL Server schema changed during the snapshot"
        );
        if let Some(boundary) = &boundary {
            let captures: Vec<&str> = catalog.tables.iter().map(|t| t.capture.as_str()).collect();
            for min in session.min_lsns(&captures).await? {
                ensure!(
                    min != [0; 10] && min <= boundary.lsn,
                    "SQL Server snapshot CDC boundary expired before completion"
                );
            }
        }
        check_cancel(&cancel)?;
        session
            .client_mut()
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;
        Ok::<_, anyhow::Error>(boundary)
    }
    .await;
    // No SQL future is abandoned on cancellation. An in-flight FETCH contains
    // at most the configured bounded batch; cleanup drains it before rollback.
    let rollback = async {
        session
            .client_mut()
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
        // A failure inside the cut leaves the marker session's table locks
        // held; release them before anything else.
        if let Some(marker) = marker.as_mut() {
            marker
                .client_mut()
                .simple_query("IF @@TRANCOUNT>0 ROLLBACK TRANSACTION")
                .await?
                .into_results()
                .await?;
        }
        if marker_attempted {
            marker
                .as_mut()
                .context("owned marker connection lost")?
                .client_mut()
                .execute(
                    format!(
                        "DELETE FROM {} WHERE token = CONVERT(uniqueidentifier, @P1)",
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
        Some(format!(
            "SQL Server snapshot cleanup not confirmed for marker {token:?}: rollback={rollback:?}, \
             marker={cleanup:?}; the source's erasing removal deletes orphaned markers"
        ))
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

/// Both snapshot modes read under SNAPSHOT isolation, which the database
/// must allow; refuse with the fix instead of SQL Server's generic error.
async fn ensure_snapshot_isolation(session: &mut TiberiusSession) -> Result<()> {
    let rows = session
        .client_mut()
        .simple_query(
            "SELECT snapshot_isolation_state, DB_NAME() FROM sys.databases WHERE database_id = DB_ID()",
        )
        .await?
        .into_first_result()
        .await?;
    let row = rows
        .first()
        .context("SQL Server database metadata missing")?;
    let state = row
        .try_get::<u8, _>(0)?
        .context("snapshot isolation state missing")?;
    let database = row.try_get::<&str, _>(1)?.unwrap_or_default();
    ensure!(
        state == 1,
        "SQL Server initial loading reads under SNAPSHOT isolation, which database {database} \
         does not allow; run ALTER DATABASE {} SET ALLOW_SNAPSHOT_ISOLATION ON",
        quoted(database)
    );
    Ok(())
}

async fn stream_tables(
    session: &mut TiberiusSession,
    settings: &NativeSettings,
    source: &str,
    labels: &[String],
    catalog: &Catalog,
    sender: &Sender,
    cancel: &watch::Receiver<bool>,
) -> Result<()> {
    let batch = settings.max_rows.min(64);
    let fetch = "FETCH NEXT FROM drasi_native_snapshot;".repeat(batch);
    let timestamp = u64::try_from(chrono::Utc::now().timestamp_millis())?;
    let client = session.client_mut();
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
                let node = element(source, table, properties(table, &row, 0)?, timestamp)?;
                drop(row);
                sender.send(SourceChange::Insert { element: node }).await?;
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
