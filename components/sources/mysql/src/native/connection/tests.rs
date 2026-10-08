// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{
    num::{NonZeroU32, NonZeroU64},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context as TaskContext, Poll},
};
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::mysql::Mysql;
use tokio::io::{duplex, ReadBuf};

#[tokio::test(flavor = "current_thread")]
async fn cancelled_reads_retain_each_header_and_payload_byte() -> Result<()> {
    let (stream, mut peer) = duplex(16);
    let mut packets = Packets::new(stream, 64, Duration::from_secs(1));
    let frame = [5, 0, 0, 0, 10, 20, 30, 40, 50];
    for byte in &frame[..frame.len() - 1] {
        peer.write_all(&[*byte]).await?;
        let read = packets.read();
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
    }
    peer.write_all(&[50]).await?;
    assert_eq!(packets.read().await?.as_ref(), &[10, 20, 30, 40, 50]);
    peer.write_all(&[1, 0, 0, 1, 60]).await?;
    assert_eq!(packets.read().await?.as_ref(), &[60]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_or_oversized_packets_fence_without_waiting_for_the_payload() -> Result<()> {
    for frame in [[65, 0, 0, 0], [1, 0, 0, 1]] {
        let (stream, mut peer) = duplex(16);
        let mut packets = Packets::new(stream, 64, Duration::from_secs(1));
        peer.write_all(&frame).await?;
        assert!(timeout(Duration::from_millis(50), packets.read())
            .await?
            .is_err());
        assert!(packets.reset_sequence().is_err());
        assert!(packets.read().await.is_err());
    }
    let (stream, mut peer) = duplex(16);
    let mut packets = Packets::new(stream, 64, Duration::from_secs(1));
    peer.write_all(&[1, 0]).await?;
    drop(peer);
    let error = packets.read().await.expect_err("truncated header");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .context("I/O cause")?
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn partial_packet_deadline_survives_cancellation_but_idle_does_not_expire() -> Result<()> {
    let (stream, mut peer) = duplex(16);
    let mut packets = Packets::new(stream, 64, Duration::from_millis(20));
    assert!(timeout(Duration::from_millis(40), packets.read())
        .await
        .is_err());
    peer.write_all(&[1]).await?;
    {
        let read = packets.read();
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(packets
        .read()
        .await
        .expect_err("partial deadline")
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_writes_resume_the_original_packet_without_interleaving() -> Result<()> {
    let (stream, mut peer) = duplex(2);
    let mut packets = Packets::new(stream, 64, Duration::from_secs(1));
    packets.queue(b"owned")?;
    {
        let write = packets.flush();
        tokio::pin!(write);
        assert!(futures_util::poll!(&mut write).is_pending());
    }
    assert!(packets.queue(b"replacement").is_err());
    assert!(packets.reset_sequence().is_err());
    let mut actual = [0; 9];
    let (write, read) = tokio::join!(packets.flush(), peer.read_exact(&mut actual));
    write?;
    read?;
    assert_eq!(&actual, b"\x05\x00\x00\x00owned");
    packets.queue(b"x")?;
    let mut actual = [0; 5];
    let (write, read) = tokio::join!(packets.flush(), peer.read_exact(&mut actual));
    write?;
    read?;
    assert_eq!(&actual, b"\x01\x00\x00\x01x");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn full_size_chunks_require_the_correct_terminator_and_respect_total_limits() -> Result<()> {
    for length in [CHUNK - 1, CHUNK, CHUNK + 1] {
        let (left, right) = duplex(64 * 1024);
        let mut sender = Packets::new(left, length, Duration::from_secs(3));
        let mut receiver = Packets::new(right, length, Duration::from_secs(3));
        let payload = vec![0x5a; length];
        sender.queue(&payload)?;
        let (write, read) = tokio::join!(sender.flush(), receiver.read());
        write?;
        assert_eq!(read?.as_ref(), payload);
        assert_eq!(sender.sequence, receiver.sequence);
        assert_eq!(receiver.sequence, if length < CHUNK { 1 } else { 2 });
    }
    Ok(())
}

struct ShutdownFailure(Arc<AtomicBool>);

impl Drop for ShutdownFailure {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl AsyncRead for ShutdownFailure {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}
impl AsyncWrite for ShutdownFailure {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
        _: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "injected shutdown failure",
        )))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_errors_keep_the_original_cause_and_release_the_owned_socket() -> Result<()> {
    let dropped = Arc::new(AtomicBool::new(false));
    let connection = Connection {
        packets: Packets::new(
            Box::new(ShutdownFailure(dropped.clone())) as Socket,
            256,
            Duration::from_secs(1),
        ),
        version: (8, 0, 36),
        connection_id: 1,
        encrypted: false,
        command_incomplete: false,
        replication: false,
    };
    let error = connection.close().await.expect_err("shutdown failure");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .context("I/O cause")?
            .kind(),
        std::io::ErrorKind::ConnectionReset
    );
    assert!(dropped.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn malformed_lengths_authentication_and_server_errors_are_explicit() -> Result<()> {
    for bytes in [&[255][..], &[252, 1][..], &[253, 1, 2][..], &[254, 1][..]] {
        assert!(length(&mut &bytes[..]).is_err());
    }
    assert!(validate_plugin(&AuthPlugin::MysqlClearPassword, &[0; 20]).is_err());
    assert!(validate_plugin(&AuthPlugin::MysqlNativePassword, &[0; 19]).is_err());
    let error = check_error(b"\xff\x15\x04#28000Access denied").expect_err("server error");
    let error = error
        .downcast_ref::<MySqlServerError>()
        .context("typed server cause")?;
    assert_eq!(error.code, 1045);
    assert_eq!(error.sql_state.as_deref(), Some("28000"));
    assert!(validate_ok(&[0, 0, 0, 8, 0, 0, 0]).is_err());
    Ok(())
}

#[test]
fn binary_cursor_rows_reject_unsupported_types_and_invalid_temporal_encodings() -> Result<()> {
    use ColumnType::*;
    for kind in [
        MYSQL_TYPE_TIMESTAMP2,
        MYSQL_TYPE_TIME2,
        MYSQL_TYPE_DATETIME2,
        MYSQL_TYPE_GEOMETRY,
        MYSQL_TYPE_VECTOR,
    ] {
        assert!(binary_row(&[0, 4], &[Column::new(kind)]).is_err());
    }
    for (kind, body) in [
        (MYSQL_TYPE_DATE, vec![1, 0]),
        (MYSQL_TYPE_DATETIME, vec![5, 0, 0, 0, 0, 0]),
        (MYSQL_TYPE_DATE, vec![4, 0xe8, 7, 13, 1]),
        (MYSQL_TYPE_TIME, vec![8, 2, 0, 0, 0, 0, 0, 0, 0]),
        (MYSQL_TYPE_TIME, vec![8, 0, 255, 255, 255, 255, 0, 0, 0]),
    ] {
        let mut packet = vec![0, 0];
        packet.extend(body);
        assert!(binary_row(&packet, &[Column::new(kind)]).is_err());
    }
    assert_eq!(
        binary_row(&[0, 0, 0], &[Column::new(MYSQL_TYPE_DATE)])?,
        vec![mysql_common::Value::Date(0, 0, 0, 0, 0, 0, 0)]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn binary_cursor_fetch_rejects_incomplete_extra_rows_and_invalid_status() -> Result<()> {
    let row = vec![0, 0, 7, 0, 0, 0];
    let end = vec![0xfe, 0, 0, 128, 0];
    for (frames, expected) in [
        (
            vec![row.clone(), end.clone()],
            Some(vec![mysql_common::Value::Int(7)]),
        ),
        (
            vec![vec![0, 4], end.clone()],
            Some(vec![mysql_common::Value::NULL]),
        ),
        (vec![vec![0]], None),
        (vec![vec![0, 0, 7]], None),
        (vec![vec![0, 0, 7, 0, 0, 0, 99]], None),
        (vec![vec![1, 0, 7, 0, 0, 0]], None),
        (vec![row.clone(), row.clone(), end], None),
        (vec![vec![0xfe, 0, 0, 64, 0]], None),
        (vec![row.clone(), vec![0xfe, 0]], None),
        (vec![row, vec![0xfe, 0, 0, 0, 0]], None),
    ] {
        let (stream, peer) = duplex(2048);
        let mut connection = Connection {
            packets: Packets::new(Box::new(stream) as Socket, 1024, Duration::from_secs(1)),
            version: (8, 0, 36),
            connection_id: 1,
            encrypted: false,
            command_incomplete: false,
            replication: false,
        };
        let mut peer = Packets::new(peer, 1024, Duration::from_secs(1));
        let mut cursor = Cursor {
            id: 17,
            columns: vec![Column::new(ColumnType::MYSQL_TYPE_LONG)],
            finished: false,
        };
        let responses = async {
            assert_eq!(
                peer.read().await?.as_ref(),
                &[0x1c, 17, 0, 0, 0, 1, 0, 0, 0]
            );
            for frame in frames {
                peer.send(&frame).await?;
            }
            Result::<()>::Ok(())
        };
        let (result, responses) = tokio::join!(connection.fetch(&mut cursor), responses);
        responses?;
        match expected {
            Some(value) => {
                assert_eq!(result?, Some(value));
                assert!(!connection.command_incomplete);
                assert!(connection.fetch(&mut cursor).await?.is_none());
            }
            None => {
                assert!(result.is_err());
                assert!(connection.execute("SELECT 1").await.is_err());
            }
        }
        connection.close().await?;
    }
    Ok(())
}

#[test]
fn greeting_rejects_every_invalid_raw_nonce_length_before_dependency_parsing() -> Result<()> {
    let mut packet = Vec::new();
    HandshakePacket::new(
        10,
        b"8.0.36".as_slice(),
        1,
        *b"12345678",
        Some(b"abcdefghijkl\0".as_slice()),
        FLAGS,
        45,
        mysql_common::constants::StatusFlags::empty(),
        Some(b"caching_sha2_password".as_slice()),
    )
    .serialize(&mut packet);
    let nonce_index = 7 + 21;
    packet[nonce_index] = 21;
    assert_eq!(greeting(&packet)?.nonce(), b"12345678abcdefghijkl");
    for length in 0..=255 {
        if length != 21 {
            packet[nonce_index] = length;
            assert!(greeting(&packet).is_err(), "nonce length {length}");
        }
    }
    packet[nonce_index] = 21;
    for end in 0..packet.len() - 1 {
        assert!(
            greeting(&packet[..end]).is_err(),
            "truncated greeting {end}"
        );
    }
    packet[7 + 44] = 1;
    assert!(greeting(&packet).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn owned_mysql_connections_authenticate_query_and_close_without_driver_tasks() -> Result<()> {
    use chrono::Datelike;
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    let today = chrono::Utc::now().date_naive();
    let expires = today + chrono::Duration::days(7);
    let before = rcgen::date_time_ymd(today.year(), today.month() as u8, today.day() as u8);
    let after = rcgen::date_time_ymd(expires.year(), expires.month() as u8, expires.day() as u8);
    let ca_key = KeyPair::generate()?;
    let mut ca = CertificateParams::new(Vec::new())?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.not_before = before;
    ca.not_after = after;
    let ca = ca.self_signed(&ca_key)?;
    let key = KeyPair::generate()?;
    let mut server = CertificateParams::new(vec!["localhost".into()])?;
    server.not_before = before;
    server.not_after = after;
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server = server.signed_by(&key, &ca, &ca_key)?;
    let container = Mysql::default()
        .with_copy_to("/tmp/native-ca.pem", ca.pem().into_bytes())
        .with_copy_to("/tmp/native-server.pem", server.pem().into_bytes())
        .with_copy_to("/tmp/native-key.pem", key.serialize_pem().into_bytes())
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_cmd(vec![
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
            "--ssl-ca=/tmp/native-ca.pem",
            "--ssl-cert=/tmp/native-server.pem",
            "--ssl-key=/tmp/native-key.pem",
        ])
        .start()
        .await?;
    let config = MySqlConnectionConfig {
        host: "127.0.0.1".into(),
        port: container.get_host_port_ipv4(3306).await?,
        database: "test".into(),
        user: "root".into(),
        password: "root".into(),
        ssl_mode: MySqlTlsMode::Disabled,
        tls_ca_pem: None,
        server_id: NonZeroU32::new(456).context("server ID")?,
        heartbeat_interval_ms: NonZeroU64::new(1000).context("heartbeat")?,
    };
    let mut admin = Connection::connect(&config, 1024 * 1024, Duration::from_secs(5)).await?;
    admin
        .execute("CREATE USER 'owned'@'%' IDENTIFIED WITH caching_sha2_password BY 'owned'")
        .await?;
    admin
        .execute("GRANT SELECT, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'owned'@'%'")
        .await?;
    for ssl_mode in [
        MySqlTlsMode::Disabled,
        MySqlTlsMode::Require,
        MySqlTlsMode::IfAvailable,
    ] {
        admin
            .execute("ALTER USER 'owned'@'%' IDENTIFIED WITH caching_sha2_password BY 'owned'")
            .await?;
        let mut settings = config.clone();
        settings.user = "owned".into();
        settings.password = "owned".into();
        settings.ssl_mode = ssl_mode;
        let mut connection =
            Connection::connect(&settings, 1024 * 1024, Duration::from_secs(5)).await?;
        assert_eq!(connection.encrypted, ssl_mode != MySqlTlsMode::Disabled);
        let rows = connection.query("SELECT 'hello', NULL, 123").await?;
        assert_eq!(
            rows.values,
            vec![vec![Some(b"hello".to_vec()), None, Some(b"123".to_vec())]]
        );
        let id = connection.connection_id;
        connection.close().await?;
        timeout(Duration::from_secs(3), async {
            loop {
                let rows = admin
                    .query(&format!(
                        "SELECT ID FROM information_schema.PROCESSLIST WHERE ID={id}"
                    ))
                    .await?;
                if rows.values.is_empty() {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
    }
    let mut verified = config.clone();
    verified.ssl_mode = MySqlTlsMode::RequireVerifyCa;
    assert!(
        Connection::connect(&verified, 1024 * 1024, Duration::from_secs(5))
            .await
            .is_err()
    );
    verified.tls_ca_pem = Some(format!("{}\0", ca.pem()));
    let connection = Connection::connect(&verified, 1024 * 1024, Duration::from_secs(5)).await?;
    assert!(connection.encrypted);
    connection.close().await?;
    verified.ssl_mode = MySqlTlsMode::RequireVerifyFull;
    assert!(
        Connection::connect(&verified, 1024 * 1024, Duration::from_secs(5))
            .await
            .is_err(),
        "localhost certificate must not validate for 127.0.0.1"
    );
    verified.host = "localhost".into();
    let connection = Connection::connect(&verified, 1024 * 1024, Duration::from_secs(5)).await?;
    assert!(connection.encrypted);
    connection.close().await?;
    let mut refused = config.clone();
    refused.password = "incorrect".into();
    let error = match Connection::connect(&refused, 1024 * 1024, Duration::from_secs(5)).await {
        Ok(connection) => {
            connection.close().await?;
            anyhow::bail!("invalid credentials accepted");
        }
        Err(error) => error,
    };
    assert_eq!(
        error
            .downcast_ref::<MySqlServerError>()
            .context("authentication refusal cause")?
            .code,
        1045
    );
    admin.close().await?;
    Ok(())
}
