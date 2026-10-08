// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use chrono::Datelike;
use drasi_source_postgres::native::PostgresSnapshot;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};

struct Certificates {
    ca: String,
    server: String,
    key: String,
}

impl Certificates {
    fn new(expired: bool) -> Result<Self> {
        let today = chrono::Utc::now().date_naive();
        let date = |day: chrono::NaiveDate| {
            rcgen::date_time_ymd(day.year(), day.month() as u8, day.day() as u8)
        };
        let ca_key = KeyPair::generate()?;
        let mut ca = CertificateParams::new(Vec::new())?;
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca.not_before = date(today - chrono::Duration::days(14));
        ca.not_after = date(today + chrono::Duration::days(7));
        let ca = ca.self_signed(&ca_key)?;
        let key = KeyPair::generate()?;
        let mut server = CertificateParams::new(vec!["localhost".into()])?;
        server.not_before = date(today - chrono::Duration::days(7));
        server.not_after = date(today + chrono::Duration::days(if expired { -1 } else { 7 }));
        server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = server.signed_by(&key, &ca, &ca_key)?;
        Ok(Self {
            ca: ca.pem(),
            server: server.pem(),
            key: key.serialize_pem(),
        })
    }

    async fn database(&self) -> Result<Database> {
        let container = GenericImage::new("postgres", "16-alpine")
            .with_exposed_port(ContainerPort::Tcp(5432))
            .with_wait_for(WaitFor::log(
                LogWaitStrategy::stdout_or_stderr("database system is ready to accept connections")
                    .with_times(2),
            ))
            .with_entrypoint("/bin/sh")
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .with_copy_to("/tmp/native-server.crt", self.server.as_bytes().to_vec())
            .with_copy_to("/tmp/native-server.key", self.key.as_bytes().to_vec())
            .with_cmd([
                "-c",
                "chown postgres:postgres /tmp/native-server.key && \
                 chmod 600 /tmp/native-server.key && \
                 exec docker-entrypoint.sh postgres -c wal_level=logical \
                 -c wal_sender_timeout=1500 -c ssl=on \
                 -c ssl_cert_file=/tmp/native-server.crt -c ssl_key_file=/tmp/native-server.key",
            ])
            .start()
            .await?;
        let mut database = Database::from_container(container).await?;
        database.config.connection.host = "localhost".into();
        database.config.connection.ssl_mode = SslMode::Require;
        database.config.tls_ca_pem = Some(self.ca.clone());
        Ok(database)
    }
}

fn paired(
    config: PostgresTransactionConfig,
    owner: Arc<QuerySourceProgress>,
    initialize: bool,
) -> Result<(PostgresTransactionSource, Option<Arc<PostgresSnapshot>>)> {
    if initialize {
        let (source, snapshot) = PostgresTransactionSource::coordinated(
            ComponentId::try_new("source")?,
            StreamId::try_new("changes")?,
            config,
            owner,
        )?;
        Ok((source, Some(snapshot)))
    } else {
        Ok((source(config, owner)?, None))
    }
}

async fn retired(database: &Database) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = database
                .client()
                .query_one(
                    "SELECT COUNT(*) FROM pg_stat_activity WHERE pid<>pg_backend_pid() \
                 AND backend_type IN ('client backend','walsender')",
                    &[],
                )
                .await?
                .get(0);
            if count == 0 {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("native TLS sessions did not retire")?
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_verified_tls_streams_initial_and_live_state_and_recovers() -> Result<()> {
    let certificates = Certificates::new(false)?;
    for initialize in [false, true] {
        let mut database = certificates.database().await?;
        if !initialize {
            database.config.connection.ssl_mode = SslMode::Prefer;
        }
        let path = tempfile::tempdir()?;
        if initialize {
            database
                .client()
                .batch_execute("INSERT INTO person VALUES (1,'initial')")
                .await?;
        }
        {
            let owner = progress();
            let (mut source, snapshot) =
                paired(database.config.clone(), owner.clone(), initialize)?;
            let mut query = query(path.path(), owner).await?;
            if let Some(snapshot) = snapshot {
                query = query.with_bootstrap(snapshot);
            }
            query.start().await?;
            assert_eq!(rows(&query).len(), usize::from(initialize));
            source.start().await?;
            let sessions = database
                .client()
                .query(
                    "SELECT s.ssl FROM pg_stat_ssl s JOIN pg_stat_activity a USING (pid) \
                 WHERE a.backend_type='walsender'",
                    &[],
                )
                .await?;
            assert!(!sessions.is_empty());
            assert!(sessions.iter().all(|row| row.get::<_, bool>(0)));
            database
                .client()
                .batch_execute("INSERT INTO person VALUES (2,'live')")
                .await?;
            apply(&mut query, next(&mut source).await?).await?;
            assert_eq!(rows(&query).len(), usize::from(initialize) + 1);
            source.stop().await?;
            query.stop().await?;
        }
        retired(&database).await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (3,'offline')")
            .await?;
        {
            let owner = progress();
            let (mut source, snapshot) =
                paired(database.config.clone(), owner.clone(), initialize)?;
            let mut query = query(path.path(), owner).await?;
            if let Some(snapshot) = snapshot {
                query = query.with_bootstrap(snapshot);
            }
            query.start().await?;
            source.start().await?;
            apply(&mut query, next(&mut source).await?).await?;
            assert_eq!(rows(&query).len(), usize::from(initialize) + 2);
            source.stop().await?;
            query.stop().await?;
        }
        retired(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_tls_rejects_untrusted_wrong_host_and_invalid_ca_without_progress() -> Result<()> {
    let certificates = Certificates::new(false)?;
    let database = certificates.database().await?;
    let unrelated = Certificates::new(false)?;
    let path = tempfile::tempdir()?;
    let owner = progress();
    let mut query = query(path.path(), owner.clone()).await?;
    query.start().await?;
    for mode in [SslMode::Prefer, SslMode::Require] {
        for (host, ca) in [
            ("localhost", None),
            ("localhost", Some(unrelated.ca.clone())),
            ("127.0.0.1", Some(certificates.ca.clone())),
            ("localhost", Some("not a certificate".into())),
        ] {
            let mut config = database.config.clone();
            config.connection.ssl_mode = mode;
            config.connection.host = host.into();
            config.tls_ca_pem = ca;
            let mut source = source(config, owner.clone())?;
            let error = source.start().await.expect_err("invalid TLS identity");
            anyhow::ensure!(
                error.downcast_ref::<native_tls::Error>().is_some(),
                "{error:#}"
            );
            source.stop().await?;
            assert!(owner.snapshot().checkpoints.is_empty());
            assert!(rows(&query).is_empty());
            retired(&database).await?;
        }
    }
    for ca in [String::new(), "x".repeat(65537), "\0".into()] {
        let mut config = database.config.clone();
        config.tls_ca_pem = Some(ca);
        assert!(source(config, owner.clone()).is_err());
    }
    let mut previous = serde_json::to_value(&database.config)?;
    previous
        .as_object_mut()
        .context("configuration object")?
        .remove("tls_ca_pem");
    assert!(
        serde_json::from_value::<PostgresTransactionConfig>(previous)?
            .tls_ca_pem
            .is_none()
    );
    query.stop().await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_tls_rejects_expired_certificates_without_plaintext_retry() -> Result<()> {
    let certificates = Certificates::new(true)?;
    let database = certificates.database().await?;
    let path = tempfile::tempdir()?;
    let owner = progress();
    let mut query = query(path.path(), owner.clone()).await?;
    query.start().await?;
    for mode in [SslMode::Prefer, SslMode::Require] {
        let mut config = database.config.clone();
        config.connection.ssl_mode = mode;
        let mut source = source(config, owner.clone())?;
        let error = source.start().await.expect_err("expired certificate");
        anyhow::ensure!(
            error.downcast_ref::<native_tls::Error>().is_some(),
            "{error:#}"
        );
        source.stop().await?;
        assert!(owner.snapshot().checkpoints.is_empty());
        retired(&database).await?;
    }
    query.stop().await?;
    database.shutdown().await
}
