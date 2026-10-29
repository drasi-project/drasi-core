use super::*;
use drasi_core::models::SourceChange;
use drasi_source_sqlite::{RestApiConfig, TableKeyConfig};
use reqwest::{Client, StatusCode};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn rest() -> RestApiConfig {
    RestApiConfig {
        host: "127.0.0.1".into(),
        port: 0,
    }
}

fn volatile(path: &Path, mode: SqliteOutput) -> SqliteConfig {
    let mut settings = config(path);
    settings.replay = None;
    settings.output = mode;
    settings.output_capacity = size(16);
    settings
}

fn changes(output: OutputEnvelope, mode: SqliteOutput) -> Result<Vec<SourceChange>> {
    Ok(match mode {
        SqliteOutput::Changes => GraphChangeCodec::decode_changes(&output.envelope)?,
        SqliteOutput::Transactions => {
            SourceTransactionCodec::decode(&output.envelope, limits())?.into_changes()
        }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn native_rest_crud_batches_and_rowids_use_the_source_connection() -> Result<()> {
    for mode in [SqliteOutput::Changes, SqliteOutput::Transactions] {
        let directory = tempfile::tempdir()?;
        let mut source = SqliteSource::new(
            id("source"),
            stream("changes"),
            volatile(directory.path(), mode),
        )?
        .with_rest_api(rest())?;
        source.start().await?;
        let handle = source.handle();
        let result = async {
            handle
                .execute_batch(
                    "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
                CREATE TABLE keyless(rowid TEXT,value TEXT);",
                )
                .await?;
            let base = format!(
                "http://{}",
                source.bound_rest_address().context("REST address")?
            );
            let client = Client::new();
            assert_eq!(
                client.get(format!("{base}/health")).send().await?.status(),
                StatusCode::OK
            );
            assert_eq!(
                client
                    .get(format!("{base}/api/tables"))
                    .send()
                    .await?
                    .json::<serde_json::Value>()
                    .await?,
                json!(["items", "keyless"]),
            );
            assert_eq!(
                client
                    .post(format!("{base}/api/tables/items"))
                    .json(&json!({"id":1,"value":"first"}))
                    .send()
                    .await?
                    .status(),
                StatusCode::OK
            );
            let first = changes(next(&mut source).await?, mode)?;
            assert!(matches!(&first[..], [SourceChange::Insert { .. }]));
            let identity = first[0].get_reference().clone();
            assert_eq!(
                client
                    .get(format!("{base}/api/tables/items/1"))
                    .send()
                    .await?
                    .json::<serde_json::Value>()
                    .await?,
                json!({"id":1,"value":"first"})
            );
            assert_eq!(
                client
                    .put(format!("{base}/api/tables/items/1"))
                    .json(&json!({"value":"updated"}))
                    .send()
                    .await?
                    .status(),
                StatusCode::OK
            );
            assert!(matches!(&changes(next(&mut source).await?, mode)?[..],
                [SourceChange::Update { element, .. }] if element.get_reference() == &identity));
            assert_eq!(
                client
                    .delete(format!("{base}/api/tables/items/1"))
                    .send()
                    .await?
                    .status(),
                StatusCode::OK
            );
            assert!(matches!(&changes(next(&mut source).await?, mode)?[..],
                [SourceChange::Delete { metadata }] if metadata.reference == identity));
            assert_eq!(
                client
                    .get(format!("{base}/api/tables/items/1"))
                    .send()
                    .await?
                    .status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                client
                    .post(format!("{base}/api/batch"))
                    .json(&json!({"operations":[
                        {"op":"insert","table":"items","data":{"id":2,"value":"initial"}},
                        {"op":"update","table":"items","id":"2","data":{"value":"final"}}
                    ]}))
                    .send()
                    .await?
                    .status(),
                StatusCode::OK
            );
            assert_eq!(changes(next(&mut source).await?, mode)?.len(), 2);
            assert_eq!(
                client
                    .post(format!("{base}/api/batch"))
                    .json(&json!({"operations":[
                        {"op":"insert","table":"items","data":{"id":3,"value":"rollback"}},
                        {"op":"insert","table":"items","data":{"id":2,"value":"duplicate"}}
                    ]}))
                    .send()
                    .await?
                    .status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
            assert_eq!(
                serde_json::to_value(handle.query("SELECT id,value FROM items").await?)?,
                json!([{"id":2,"value":"final"}])
            );
            assert!(
                timeout(Duration::from_millis(20), source.next())
                    .await
                    .is_err(),
                "failed REST batch cannot publish a prefix"
            );
            assert_eq!(
                client
                    .post(format!("{base}/api/tables/keyless"))
                    .json(&json!({"rowid":"user-column","value":"keyless"}))
                    .send()
                    .await?
                    .status(),
                StatusCode::OK
            );
            assert!(changes(next(&mut source).await?, mode)?[0]
                .get_reference()
                .element_id
                .starts_with("sqlite:rowid:v1:"));
            assert_eq!(
                client
                    .get(format!("{base}/api/tables/keyless/1"))
                    .send()
                    .await?
                    .json::<serde_json::Value>()
                    .await?,
                json!({"rowid":"user-column","value":"keyless"})
            );
            assert_eq!(
                client
                    .get(format!("{base}/api/tables/__drasi_native_state_v1"))
                    .send()
                    .await?
                    .status(),
                StatusCode::BAD_REQUEST
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = source.stop().await;
        result?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_rest_honors_configured_keys_and_table_filters() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut settings = volatile(directory.path(), SqliteOutput::Changes);
    settings.tables = vec!["items".into()];
    settings.table_keys = vec![TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["code".into()],
    }];
    let mut source =
        SqliteSource::new(id("source"), stream("changes"), settings)?.with_rest_api(rest())?;
    source.start().await?;
    let result = async {
        source
            .handle()
            .execute_batch(
                "CREATE TABLE items(id INTEGER PRIMARY KEY,code TEXT,value TEXT);
            CREATE TABLE hidden(id INTEGER PRIMARY KEY);",
            )
            .await?;
        let base = format!(
            "http://{}",
            source.bound_rest_address().context("REST address")?
        );
        let client = Client::new();
        assert_eq!(
            client
                .get(format!("{base}/api/tables"))
                .send()
                .await?
                .json::<serde_json::Value>()
                .await?,
            json!(["items"])
        );
        assert_eq!(
            client
                .get(format!("{base}/api/tables/hidden"))
                .send()
                .await?
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tables/items"))
                .json(&json!({
                    "id":1, "code":"natural-key", "value":"before"
                }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        next(&mut source).await?;
        assert_eq!(
            client
                .get(format!("{base}/api/tables/items/1"))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .put(format!("{base}/api/tables/items/natural-key"))
                .json(&json!({"code":"new-key","value":"after"}))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            GraphChangeCodec::decode_changes(&next(&mut source).await?.envelope)?.len(),
            2
        );
        assert_eq!(
            client
                .get(format!("{base}/api/tables/items/new-key"))
                .send()
                .await?
                .json::<serde_json::Value>()
                .await?,
            json!({"id":1,"code":"new-key","value":"after"})
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = source.stop().await;
    result?;
    cleanup
}

#[tokio::test(flavor = "current_thread")]
async fn native_rest_writes_feed_the_actual_persistent_query_and_hide_journals() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let owner = progress();
    let mut consumer = query(directory.path(), owner.clone()).await?;
    consumer.start().await?;
    let mut producer = source(config(directory.path()), owner)?.with_rest_api(rest())?;
    producer.start().await?;
    producer
        .handle()
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)")
        .await?;
    let base = format!(
        "http://{}",
        producer.bound_rest_address().context("REST address")?
    );
    let client = Client::new();
    assert_eq!(
        client
            .get(format!("{base}/api/tables"))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?,
        json!(["items"])
    );
    assert_eq!(
        client
            .post(format!("{base}/api/tables/items"))
            .json(&json!({"id":1,"value":"durable"}))
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    apply(&mut consumer, next(&mut producer).await?).await?;
    assert_eq!(rows(&consumer), [json!({"id":1,"value":"durable"})]);
    producer.stop().await?;
    consumer.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_rest_cancelled_and_timed_out_stop_retain_accepted_requests() -> Result<()> {
    for cancel in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut settings = volatile(directory.path(), SqliteOutput::Changes);
        settings.shutdown_timeout_ms = NonZeroU64::new(80).context("cleanup timeout")?;
        let mut source =
            SqliteSource::new(id("source"), stream("changes"), settings)?.with_rest_api(rest())?;
        source.start().await?;
        let handle = source.handle();
        handle
            .execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)")
            .await?;
        let address = source.bound_rest_address().context("REST address")?;
        let mut request = tokio::net::TcpStream::connect(address).await?;
        let body = r#"{"id":1,"value":"accepted"}"#;
        request.write_all(format!(
            "POST /api/tables/items HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            body.len(),
        ).as_bytes()).await?;
        let mut interim = [0; 25];
        timeout(Duration::from_secs(2), request.read_exact(&mut interim)).await??;
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        if cancel {
            assert!(timeout(Duration::from_millis(20), source.stop())
                .await
                .is_err());
        } else {
            assert!(source
                .stop()
                .await
                .expect_err("unfinished request must retain cleanup")
                .is::<drasi_lib::context::workers::WorkerCleanupError>());
        }
        assert!(
            source.start().await.is_err(),
            "unfinished REST owner cannot be replaced"
        );
        assert!(
            handle.query("SELECT 1").await.is_err(),
            "new SQL admission is closed"
        );
        request.write_all(body.as_bytes()).await?;
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), request.read_to_end(&mut response)).await??;
        let response = String::from_utf8(response)?;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert_eq!(
            GraphChangeCodec::decode_changes(&next(&mut source).await?.envelope)?.len(),
            1
        );
        source.stop().await?;
        assert!(source.bound_rest_address().is_none());
        let released = tokio::net::TcpListener::bind(address).await?;
        drop(released);
        source.start().await?;
        assert_eq!(
            serde_json::to_value(handle.query("SELECT id,value FROM items").await?)?,
            json!([{"id":1,"value":"accepted"}])
        );
        source.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_rest_bind_failure_does_not_create_or_admit_sql_work() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = occupied.local_addr()?;
    let settings = volatile(directory.path(), SqliteOutput::Changes);
    let path = settings.path.clone().context("database path")?;
    let mut source = SqliteSource::new(id("source"), stream("changes"), settings)?.with_rest_api(
        RestApiConfig {
            host: address.ip().to_string(),
            port: address.port(),
        },
    )?;
    assert!(source.start().await.is_err());
    assert!(!path.exists());
    assert!(source
        .handle()
        .execute("CREATE TABLE unexpected(id INTEGER)")
        .await
        .is_err());
    assert!(source.start().await.is_err());
    source.stop().await?;
    drop(occupied);
    source.start().await?;
    assert_eq!(source.bound_rest_address(), Some(address));
    source.stop().await?;
    Ok(())
}
