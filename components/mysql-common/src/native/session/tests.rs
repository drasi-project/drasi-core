// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::native::{tests::config, MySqlOutput};
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::mysql::Mysql;

#[test]
fn ownership_requires_the_exact_immutable_handshake_attribute() -> Result<()> {
    let rows = |values| Rows {
        columns: Vec::new(),
        values,
    };
    assert!(!validate_owner(&rows(Vec::new()), 12, "owned")?);
    assert!(validate_owner(
        &rows(vec![vec![Some(b"12".to_vec()), Some(b"owned".to_vec())]]),
        12,
        "owned",
    )?);
    for invalid in [
        vec![vec![Some(b"12".to_vec()), None]],
        vec![vec![Some(b"12".to_vec()), Some(b"unrelated".to_vec())]],
        vec![vec![Some(b"13".to_vec()), Some(b"owned".to_vec())]],
        vec![vec![Some(b"12".to_vec())]],
        vec![
            vec![Some(b"12".to_vec()), Some(b"owned".to_vec())],
            vec![Some(b"12".to_vec()), Some(b"owned".to_vec())],
        ],
    ] {
        assert!(validate_owner(&rows(invalid), 12, "owned").is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL"]
async fn missing_session_attributes_fail_before_accepting_sql_work() -> Result<()> {
    let database = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_cmd(vec!["--performance-schema-session-connect-attrs-size=0"])
        .start()
        .await?;
    let settings = config(
        database.get_host_port_ipv4(3306).await?,
        MySqlOutput::Changes,
    );
    let failure = connect(&settings.connection, 1 << 20, settings.timeout())
        .await
        .err()
        .context("missing ownership attributes must reject SQL session setup")?;
    assert!(failure
        .to_string()
        .contains("ownership attribute is missing"));
    let mut observer =
        Connection::connect(&settings.connection, 1 << 20, settings.timeout()).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows = observer
                .query("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root'")
                .await?;
            if field(&rows.values[0], 0)? == "1" {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    observer.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL"]
async fn limited_account_cleanup_uses_its_own_session_attributes() -> Result<()> {
    let database = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .start()
        .await?;
    let mut settings = config(
        database.get_host_port_ipv4(3306).await?,
        MySqlOutput::Changes,
    );
    let mut admin = Connection::connect(&settings.connection, 1 << 20, settings.timeout()).await?;
    admin
        .execute("CREATE USER 'snapshot_reader'@'%' IDENTIFIED BY 'snapshot-password'")
        .await?;
    admin
        .execute("GRANT SELECT ON test.* TO 'snapshot_reader'@'%'")
        .await?;
    settings.connection.user = "snapshot_reader".into();
    settings.connection.password = "snapshot-password".into();
    let mut control = Some(connect(&settings.connection, 1 << 20, settings.timeout()).await?);
    let server = Some(server_id(control.as_mut().unwrap()).await?);
    let mut sessions = vec![connect(&settings.connection, 1 << 20, settings.timeout()).await?];
    let marker = sessions[0].session_marker.as_ref().unwrap();
    let lock = format!("native-test-{marker}");
    sessions[0]
        .query(&format!("SELECT GET_LOCK('{lock}',0)"))
        .await?;
    let mut killed = BTreeSet::new();
    SessionCleanup {
        config: &settings.connection,
        max_bytes: 1 << 20,
        timeout: settings.timeout(),
        control: &mut control,
        sessions: &mut sessions,
        server: &server,
        killed: &mut killed,
    }
    .cleanup()
    .await?;
    assert!(sessions.is_empty());
    assert!(control.is_none());
    assert_eq!(killed.len(), 1);
    assert_eq!(
        admin
            .query(&format!("SELECT IS_USED_LOCK('{lock}')"))
            .await?
            .values,
        [vec![None]]
    );
    admin.close().await?;
    Ok(())
}

async fn wait_for_connection(settings: &crate::native::MySqlConfig) -> Result<Connection> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        match connect(&settings.connection, 1 << 20, settings.timeout()).await {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error.context("MySQL did not become ready after restart"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn restarted_connection_id_reuse() -> Result<()> {
    let database = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .start()
        .await?;
    let settings = config(
        database.get_host_port_ipv4(3306).await?,
        MySqlOutput::Changes,
    );
    let mut control = Some(connect(&settings.connection, 1 << 20, settings.timeout()).await?);
    let server = Some(server_id(control.as_mut().unwrap()).await?);
    let owned = loop {
        let connection = connect(&settings.connection, 1 << 20, settings.timeout()).await?;
        if connection.connection_id >= 128 {
            break connection;
        }
        connection.close().await?;
    };
    let old_id = owned.connection_id;
    let mut sessions = vec![owned];
    control.take().unwrap().close().await?;
    let restarted = tokio::process::Command::new("podman")
        .args(["restart", database.id()])
        .kill_on_drop(true)
        .output()
        .await?;
    anyhow::ensure!(
        restarted.status.success(),
        "owned MySQL same-data-directory restart failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let mut unrelated = wait_for_connection(&settings).await?;
    assert_eq!(Some(server_id(&mut unrelated).await?), server);
    loop {
        anyhow::ensure!(
            unrelated.connection_id <= old_id,
            "test failed to reproduce numeric connection ID reuse"
        );
        if unrelated.connection_id == old_id {
            break;
        }
        unrelated.close().await?;
        unrelated = connect(&settings.connection, 1 << 20, settings.timeout()).await?;
    }
    assert_ne!(unrelated.session_marker, sessions[0].session_marker);
    let mut killed = BTreeSet::new();
    for _ in 0..2 {
        let error = SessionCleanup {
            config: &settings.connection,
            max_bytes: 1 << 20,
            timeout: settings.timeout(),
            control: &mut control,
            sessions: &mut sessions,
            server: &server,
            killed: &mut killed,
        }
        .retire()
        .await
        .expect_err("reused ID must not authorize KILL");
        assert!(error.to_string().contains("ownership changed"));
        assert!(killed.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            unrelated.query("SELECT 1").await?.values,
            [vec![Some(b"1".to_vec())]]
        );
        control.take().unwrap().close().await?;
    }
    unrelated.close().await?;
    tokio::time::timeout(
        Duration::from_secs(15),
        SessionCleanup {
            config: &settings.connection,
            max_bytes: 1 << 20,
            timeout: settings.timeout(),
            control: &mut control,
            sessions: &mut sessions,
            server: &server,
            killed: &mut killed,
        }
        .cleanup(),
    )
    .await??;
    assert!(sessions.is_empty());
    assert!(control.is_none());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL"]
async fn restarted_connection_id_reuse_current_thread() -> Result<()> {
    restarted_connection_id_reuse().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Podman MySQL"]
async fn restarted_connection_id_reuse_multi_thread() -> Result<()> {
    restarted_connection_id_reuse().await
}
