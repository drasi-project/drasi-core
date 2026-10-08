// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use bytes::Bytes;
use mysql_common::{
    constants::{CapabilityFlags, ColumnType},
    io::ParseBuf,
    packets::{
        AuthPlugin, AuthSwitchRequest, Column, ComBinlogDump, ComRegisterSlave, ErrPacket,
        HandshakePacket, HandshakeResponse, SslRequest,
    },
    proto::MySerialize,
};
use rsa::{pkcs8::DecodePublicKey, traits::PublicKeyParts, Oaep, RsaPublicKey};
use std::{fmt, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{timeout, timeout_at, Instant},
};

use super::{MySqlConnectionConfig, MySqlTlsMode};

const CHUNK: usize = 0xff_ffff;
const FLAGS: CapabilityFlags = CapabilityFlags::CLIENT_PROTOCOL_41
    .union(CapabilityFlags::CLIENT_LONG_PASSWORD)
    .union(CapabilityFlags::CLIENT_LONG_FLAG)
    .union(CapabilityFlags::CLIENT_SECURE_CONNECTION)
    .union(CapabilityFlags::CLIENT_PLUGIN_AUTH)
    .union(CapabilityFlags::CLIENT_CONNECT_WITH_DB)
    .union(CapabilityFlags::CLIENT_TRANSACTIONS);

#[derive(Debug)]
pub struct MySqlServerError {
    pub code: u16,
    pub sql_state: Option<String>,
    pub message: String,
}

impl fmt::Display for MySqlServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MySQL error {}: {}", self.code, self.message)
    }
}
impl std::error::Error for MySqlServerError {}

fn validate_binary_type(kind: ColumnType) -> Result<()> {
    use ColumnType::*;
    // The dependency panics for recognized types outside its binary decoder.
    anyhow::ensure!(
        matches!(
            kind,
            MYSQL_TYPE_TINY
                | MYSQL_TYPE_SHORT
                | MYSQL_TYPE_LONG
                | MYSQL_TYPE_INT24
                | MYSQL_TYPE_LONGLONG
                | MYSQL_TYPE_FLOAT
                | MYSQL_TYPE_DOUBLE
                | MYSQL_TYPE_NEWDECIMAL
                | MYSQL_TYPE_YEAR
                | MYSQL_TYPE_DATE
                | MYSQL_TYPE_NEWDATE
                | MYSQL_TYPE_TIME
                | MYSQL_TYPE_DATETIME
                | MYSQL_TYPE_TIMESTAMP
                | MYSQL_TYPE_STRING
                | MYSQL_TYPE_VAR_STRING
                | MYSQL_TYPE_VARCHAR
                | MYSQL_TYPE_BLOB
                | MYSQL_TYPE_TINY_BLOB
                | MYSQL_TYPE_MEDIUM_BLOB
                | MYSQL_TYPE_LONG_BLOB
                | MYSQL_TYPE_ENUM
                | MYSQL_TYPE_SET
                | MYSQL_TYPE_BIT
                | MYSQL_TYPE_JSON
                | MYSQL_TYPE_NULL
        ),
        "unsupported MySQL cursor column type {kind:?}"
    );
    Ok(())
}

fn binary_row(packet: &[u8], columns: &[Column]) -> Result<Vec<mysql_common::Value>> {
    use mysql_common::Value;
    use ColumnType::*;
    anyhow::ensure!(
        !columns.is_empty() && columns.len() <= 4096 && packet.first() == Some(&0),
        "invalid MySQL binary row"
    );
    let bitmap_size = (columns.len() + 2).div_ceil(8);
    let bitmap = packet
        .get(1..1 + bitmap_size)
        .context("truncated row null bitmap")?;
    let mut input = ParseBuf(&packet[1 + bitmap_size..]);
    let mut row = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        let kind = column.column_type();
        validate_binary_type(kind)?;
        if bitmap[(index + 2) / 8] & (1 << ((index + 2) % 8)) != 0 {
            row.push(Value::NULL);
            continue;
        }
        let valid_length = match kind {
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => input
                .0
                .first()
                .is_some_and(|length| matches!(length, 0 | 4)),
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => input
                .0
                .first()
                .is_some_and(|length| matches!(length, 0 | 4 | 7 | 11)),
            MYSQL_TYPE_TIME => {
                input
                    .0
                    .first()
                    .is_some_and(|length| matches!(length, 0 | 8 | 12))
                    && (input.0.first() == Some(&0)
                        || input.0.get(1).is_some_and(|sign| *sign <= 1))
            }
            _ => true,
        };
        anyhow::ensure!(valid_length, "invalid MySQL binary temporal length or sign");
        let value = input
            .parse::<mysql_common::value::ValueDeserializer<mysql_common::value::BinValue>>((
                kind,
                column.flags(),
            ))?
            .0;
        let valid = match &value {
            Value::Date(year, month, day, hour, minute, second, micros) => {
                *year <= 9999
                    && *month <= 12
                    && *day <= 31
                    && *hour < 24
                    && *minute < 60
                    && *second < 60
                    && *micros < 1_000_000
            }
            Value::Time(_, days, hour, minute, second, micros) => {
                *days <= 34
                    && (*days < 34 || *hour <= 22)
                    && *hour < 24
                    && *minute < 60
                    && *second < 60
                    && *micros < 1_000_000
            }
            _ => true,
        };
        anyhow::ensure!(valid, "invalid MySQL binary temporal value");
        row.push(value);
    }
    anyhow::ensure!(input.0.is_empty(), "trailing MySQL binary row data");
    Ok(row)
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}
type Socket = Box<dyn Io>;

/// One owner retains every partial packet and write across cancelled awaits.
struct Packets<S> {
    stream: S,
    limit: usize,
    timeout: Duration,
    sequence: u8,
    header: [u8; 4],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
    chunk_length: Option<usize>,
    read_started: Option<Instant>,
    write: Vec<u8>,
    written: usize,
    write_started: Option<Instant>,
    failed: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Packets<S> {
    fn new(stream: S, limit: usize, timeout: Duration) -> Self {
        Self {
            stream,
            limit,
            timeout,
            sequence: 0,
            header: [0; 4],
            header_read: 0,
            payload: Vec::new(),
            payload_read: 0,
            chunk_length: None,
            read_started: None,
            write: Vec::new(),
            written: 0,
            write_started: None,
            failed: false,
        }
    }

    fn reset_sequence(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.failed && self.read_started.is_none() && self.write_started.is_none(),
            "MySQL connection has failed or incomplete packet I/O"
        );
        self.sequence = 0;
        Ok(())
    }

    async fn fragment(
        stream: &mut S,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize> {
        let count = match deadline {
            Some(deadline) => timeout_at(deadline, stream.read(buffer))
                .await
                .context("MySQL partial packet timed out")??,
            None => stream.read(buffer).await?,
        };
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "MySQL connection closed during packet I/O",
            )
            .into());
        }
        Ok(count)
    }

    async fn read(&mut self) -> Result<Bytes> {
        anyhow::ensure!(!self.failed, "MySQL packet connection is fenced");
        let result = self.read_inner().await;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    async fn read_inner(&mut self) -> Result<Bytes> {
        loop {
            while self.header_read < 4 {
                self.header_read += Self::fragment(
                    &mut self.stream,
                    &mut self.header[self.header_read..],
                    self.read_started.map(|start| start + self.timeout),
                )
                .await?;
                self.read_started.get_or_insert_with(Instant::now);
            }
            if self.chunk_length.is_none() {
                anyhow::ensure!(
                    self.header[3] == self.sequence,
                    "MySQL packet sequence mismatch"
                );
                let length = usize::from(self.header[0])
                    | (usize::from(self.header[1]) << 8)
                    | (usize::from(self.header[2]) << 16);
                anyhow::ensure!(
                    length <= self.limit.saturating_sub(self.payload.len()),
                    "MySQL packet exceeds protocol byte limit"
                );
                self.sequence = self.sequence.wrapping_add(1);
                self.payload.resize(self.payload.len() + length, 0);
                self.chunk_length = Some(length);
            }
            while self.payload_read < self.payload.len() {
                self.payload_read += Self::fragment(
                    &mut self.stream,
                    &mut self.payload[self.payload_read..],
                    self.read_started.map(|start| start + self.timeout),
                )
                .await?;
            }
            let length = self.chunk_length.take().context("MySQL packet length")?;
            self.header_read = 0;
            if length < CHUNK {
                self.payload_read = 0;
                self.read_started = None;
                return Ok(Bytes::from(std::mem::take(&mut self.payload)));
            }
        }
    }

    fn queue(&mut self, payload: &[u8]) -> Result<()> {
        anyhow::ensure!(!self.failed, "MySQL packet connection is fenced");
        anyhow::ensure!(self.write_started.is_none(), "MySQL write is still pending");
        anyhow::ensure!(
            payload.len() <= self.limit,
            "MySQL write exceeds protocol byte limit"
        );
        self.write.clear();
        self.written = 0;
        let mut remaining = payload;
        loop {
            let length = remaining.len().min(CHUNK);
            self.write.extend_from_slice(&[
                length as u8,
                (length >> 8) as u8,
                (length >> 16) as u8,
                self.sequence,
            ]);
            self.sequence = self.sequence.wrapping_add(1);
            self.write.extend_from_slice(&remaining[..length]);
            remaining = &remaining[length..];
            if length < CHUNK {
                break;
            }
        }
        self.write_started = Some(Instant::now());
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        anyhow::ensure!(!self.failed, "MySQL packet connection is fenced");
        let start = self.write_started.context("no pending MySQL packet")?;
        let result = timeout_at(start + self.timeout, async {
            while self.written < self.write.len() {
                let count = self.stream.write(&self.write[self.written..]).await?;
                if count == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "MySQL packet write made no progress",
                    ));
                }
                self.written += count;
            }
            self.stream.flush().await
        })
        .await
        .context("MySQL packet write timed out")
        .and_then(|result| result.map_err(Into::into));
        if result.is_ok() {
            self.write.clear();
            self.write_started = None;
        } else {
            self.failed = true;
        }
        result
    }

    async fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.queue(payload)?;
        self.flush().await
    }
}

pub(super) struct Connection {
    packets: Packets<Socket>,
    pub version: (u16, u16, u16),
    pub connection_id: u32,
    pub encrypted: bool,
    command_incomplete: bool,
    replication: bool,
}

pub(super) struct Rows {
    pub columns: Vec<Column>,
    pub values: Vec<Vec<Option<Vec<u8>>>>,
}

pub(super) struct Cursor {
    id: u32,
    pub columns: Vec<Column>,
    finished: bool,
}

impl Connection {
    pub async fn connect(
        config: &MySqlConnectionConfig,
        limit: usize,
        io_timeout: Duration,
    ) -> Result<Self> {
        timeout(io_timeout, Self::connect_inner(config, limit, io_timeout))
            .await
            .context("MySQL connection startup timed out")?
    }

    async fn connect_inner(
        config: &MySqlConnectionConfig,
        limit: usize,
        io_timeout: Duration,
    ) -> Result<Self> {
        let socket = TcpStream::connect((config.host.as_str(), config.port)).await?;
        socket.set_nodelay(true)?;
        let mut packets = Packets::new(Box::new(socket) as Socket, limit, io_timeout);
        let initial = packets.read().await?;
        check_error(&initial)?;
        let hello = greeting(&initial)?;
        anyhow::ensure!(
            hello.protocol_version() == 10
                && hello.maria_db_server_version_parsed().is_none()
                && hello.capabilities().contains(FLAGS),
            "native replication requires MySQL protocol 4.1 with plugin authentication"
        );
        let version = hello
            .server_version_parsed()
            .context("invalid MySQL server version")?;
        anyhow::ensure!(
            version >= (8, 0, 20),
            "native replication requires MySQL 8.0.20 or newer"
        );
        let mut plugin = AuthPlugin::from_bytes(
            hello
                .auth_plugin_name_ref()
                .context("missing MySQL authentication plugin")?,
        )
        .into_owned();
        let mut nonce = hello.nonce();
        validate_plugin(&plugin, &nonce)?;
        let encrypted = config.ssl_mode != MySqlTlsMode::Disabled
            && hello.capabilities().contains(CapabilityFlags::CLIENT_SSL);
        anyhow::ensure!(
            encrypted
                || matches!(
                    config.ssl_mode,
                    MySqlTlsMode::Disabled | MySqlTlsMode::IfAvailable
                ),
            "MySQL server refused required TLS"
        );
        let flags = if encrypted {
            FLAGS | CapabilityFlags::CLIENT_SSL
        } else {
            FLAGS
        };
        if encrypted {
            let mut request = Vec::new();
            SslRequest::new(flags, limit.try_into()?, 45).serialize(&mut request);
            packets.send(&request).await?;
            let mut connector = native_tls::TlsConnector::builder();
            if matches!(
                config.ssl_mode,
                MySqlTlsMode::IfAvailable | MySqlTlsMode::Require
            ) {
                connector.danger_accept_invalid_certs(true);
            }
            if config.ssl_mode != MySqlTlsMode::RequireVerifyFull {
                connector.danger_accept_invalid_hostnames(true);
            }
            if let Some(pem) = &config.tls_ca_pem {
                // MySQL-generated CA files can include a terminal C-string NUL.
                let pem = pem.strip_suffix('\0').unwrap_or(pem);
                let (label, certificate) = rsa::pkcs8::der::Document::from_pem(pem)
                    .context("invalid MySQL CA certificate PEM")?;
                anyhow::ensure!(
                    label == "CERTIFICATE",
                    "MySQL CA PEM must contain a certificate"
                );
                connector.add_root_certificate(
                    native_tls::Certificate::from_der(certificate.as_bytes())
                        .context("invalid MySQL CA certificate")?,
                );
            }
            let connector = tokio_native_tls::TlsConnector::from(connector.build()?);
            packets.stream = Box::new(connector.connect(&config.host, packets.stream).await?);
        }
        let scramble = plugin.gen_data(Some(&config.password), &nonce);
        let mut response = Vec::new();
        HandshakeResponse::new(
            scramble.as_deref(),
            version,
            Some(config.user.as_bytes()),
            Some(config.database.as_bytes()),
            Some(plugin.clone()),
            flags,
            None,
            limit.try_into()?,
        )
        .serialize(&mut response);
        packets.send(&response).await?;
        for _ in 0..8 {
            let message = packets.read().await?;
            check_error(&message)?;
            match message.as_ref() {
                [0x00, ..] => {
                    validate_ok(&message)?;
                    return Ok(Self {
                        packets,
                        version,
                        connection_id: hello.connection_id(),
                        encrypted,
                        command_incomplete: false,
                        replication: false,
                    });
                }
                [0xfe, ..] => {
                    let mut buffer = ParseBuf(&message);
                    let switch: AuthSwitchRequest<'_> = buffer.parse(())?;
                    plugin = switch.auth_plugin().into_owned();
                    nonce = switch.plugin_data().to_vec();
                    validate_plugin(&plugin, &nonce)?;
                    let data = plugin.gen_data(Some(&config.password), &nonce);
                    packets.send(data.as_deref().unwrap_or(&[])).await?;
                }
                [0x01, 0x03] if plugin == AuthPlugin::CachingSha2Password => {}
                [0x01, 0x04] if plugin == AuthPlugin::CachingSha2Password => {
                    let mut password = config.password.as_bytes().to_vec();
                    password.push(0);
                    if !encrypted {
                        packets.send(&[0x02]).await?;
                        let key = packets.read().await?;
                        check_error(&key)?;
                        anyhow::ensure!(
                            key.first() == Some(&1) && key.len() <= 16 * 1024,
                            "invalid or oversized MySQL RSA key response"
                        );
                        let key =
                            RsaPublicKey::from_public_key_pem(std::str::from_utf8(&key[1..])?)?;
                        anyhow::ensure!(
                            (2048..=4096).contains(&key.n().bits()) && key.e().bits() <= 32,
                            "unsupported MySQL RSA key size or exponent"
                        );
                        for (index, byte) in password.iter_mut().enumerate() {
                            *byte ^= nonce[index % nonce.len()];
                        }
                        password = key.encrypt(
                            &mut rsa::rand_core::OsRng,
                            Oaep::new::<sha1::Sha1>(),
                            &password,
                        )?;
                    }
                    packets.send(&password).await?;
                }
                _ => anyhow::bail!("unsupported MySQL authentication response"),
            }
        }
        anyhow::bail!("MySQL authentication exceeded the exchange limit")
    }

    async fn command(&mut self, bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(
            !self.command_incomplete && !self.replication,
            "MySQL connection is not ready for a command"
        );
        self.command_incomplete = true;
        self.packets.reset_sequence()?;
        self.packets.send(bytes).await
    }

    pub async fn query(&mut self, sql: &str) -> Result<Rows> {
        timeout(self.packets.timeout, self.query_inner(sql))
            .await
            .context("MySQL query timed out")?
    }

    async fn query_inner(&mut self, sql: &str) -> Result<Rows> {
        anyhow::ensure!(
            sql.len() < self.packets.limit,
            "MySQL query exceeds protocol byte limit"
        );
        let mut command = Vec::with_capacity(sql.len() + 1);
        command.push(3);
        command.extend_from_slice(sql.as_bytes());
        self.command(&command).await?;
        let first = self.packets.read().await?;
        if let Err(error) = check_error(&first) {
            if error.downcast_ref::<MySqlServerError>().is_some() {
                self.command_incomplete = false;
            }
            return Err(error);
        }
        if first.first() == Some(&0) {
            validate_ok(&first)?;
            self.command_incomplete = false;
            return Ok(Rows {
                columns: Vec::new(),
                values: Vec::new(),
            });
        }
        let mut first = first.as_ref();
        let count = length(&mut first)?.context("MySQL LOCAL INFILE is unsupported")?;
        anyhow::ensure!(
            first.is_empty() && count > 0 && count <= self.packets.limit,
            "invalid MySQL column count"
        );
        let mut result = Rows {
            columns: Vec::new(),
            values: Vec::new(),
        };
        let mut retained = 0usize;
        for _ in 0..count {
            let packet = self.packets.read().await?;
            check_error(&packet)?;
            account(&mut retained, packet.len(), self.packets.limit)?;
            let mut buffer = ParseBuf(&packet);
            let column: Column = buffer.parse(())?;
            anyhow::ensure!(buffer.0.is_empty(), "trailing MySQL column metadata");
            result.columns.push(column);
        }
        let end = self.packets.read().await?;
        check_error(&end)?;
        validate_eof(&end)?;
        loop {
            let packet = self.packets.read().await?;
            check_error(&packet)?;
            if packet.first() == Some(&0xfe) && packet.len() < 9 {
                validate_eof(&packet)?;
                self.command_incomplete = false;
                return Ok(result);
            }
            account(&mut retained, packet.len(), self.packets.limit)?;
            let mut input = packet.as_ref();
            let mut row = Vec::new();
            for _ in 0..count {
                row.push(match length(&mut input)? {
                    None => None,
                    Some(length) => {
                        anyhow::ensure!(length <= input.len(), "truncated MySQL row");
                        let value = input[..length].to_vec();
                        input = &input[length..];
                        Some(value)
                    }
                });
            }
            anyhow::ensure!(input.is_empty(), "trailing MySQL row data");
            result.values.push(row);
        }
    }

    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        anyhow::ensure!(
            self.query(sql).await?.columns.is_empty(),
            "unexpected MySQL query result"
        );
        Ok(())
    }

    async fn columns(&mut self, count: usize) -> Result<Vec<Column>> {
        anyhow::ensure!(
            count > 0 && count <= 4096,
            "invalid MySQL cursor column count"
        );
        let mut columns = Vec::new();
        let mut retained = 0;
        for _ in 0..count {
            let packet = self.packets.read().await?;
            check_error(&packet)?;
            account(&mut retained, packet.len(), self.packets.limit)?;
            let mut input = ParseBuf(&packet);
            let column: Column = input.parse(())?;
            validate_binary_type(column.column_type())?;
            columns.push(column);
            anyhow::ensure!(input.0.is_empty(), "trailing cursor column metadata");
        }
        Ok(columns)
    }

    pub async fn cursor(&mut self, sql: &str) -> Result<Cursor> {
        timeout(self.packets.timeout, async {
            let mut prepare = vec![0x16];
            prepare.extend(sql.as_bytes());
            self.command(&prepare).await?;
            let packet = self.packets.read().await?;
            check_error(&packet)?;
            anyhow::ensure!(
                packet.len() == 12 && packet[0] == 0 && packet[7..10] == [0, 0, 0],
                "invalid parameter-free MySQL statement preparation"
            );
            let id = u32::from_le_bytes(packet[1..5].try_into()?);
            let count = usize::from(u16::from_le_bytes(packet[5..7].try_into()?));
            let _prepared_columns = self.columns(count).await?;
            validate_eof(&self.packets.read().await?)?;
            self.command_incomplete = false;
            let mut execute = vec![0x17];
            execute.extend(id.to_le_bytes());
            execute.push(1); // Server-side read-only cursor; never buffer the complete table.
            execute.extend(1u32.to_le_bytes());
            self.command(&execute).await?;
            let first = self.packets.read().await?;
            check_error(&first)?;
            let mut input = first.as_ref();
            anyhow::ensure!(
                length(&mut input)? == Some(count) && input.is_empty(),
                "cursor column count changed"
            );
            let columns = self.columns(count).await?;
            let end = self.packets.read().await?;
            check_error(&end)?;
            validate_eof(&end)?;
            anyhow::ensure!(
                u16::from_le_bytes([end[3], end[4]]) & 64 != 0,
                "MySQL did not establish a cursor"
            );
            self.command_incomplete = false;
            Ok(Cursor {
                id,
                columns,
                finished: false,
            })
        })
        .await
        .context("MySQL cursor startup timed out")?
    }

    pub async fn fetch(&mut self, cursor: &mut Cursor) -> Result<Option<Vec<mysql_common::Value>>> {
        if cursor.finished {
            return Ok(None);
        }
        timeout(self.packets.timeout, async {
            let mut command = vec![0x1c];
            command.extend(cursor.id.to_le_bytes());
            command.extend(1u32.to_le_bytes());
            self.command(&command).await?;
            let mut value = None;
            loop {
                let packet = self.packets.read().await?;
                check_error(&packet)?;
                if packet.first() == Some(&0xfe) && packet.len() < 9 {
                    validate_eof(&packet)?;
                    let status = u16::from_le_bytes([packet[3], packet[4]]);
                    cursor.finished = status & 128 != 0;
                    anyhow::ensure!(
                        cursor.finished || (status & 64 != 0 && value.is_some()),
                        "cursor made no progress"
                    );
                    self.command_incomplete = false;
                    return Ok(value);
                }
                anyhow::ensure!(value.is_none(), "MySQL cursor returned more than one row");
                value = Some(binary_row(&packet, &cursor.columns)?);
            }
        })
        .await
        .context("MySQL cursor fetch timed out")?
    }

    pub async fn close_cursor(&mut self, cursor: Cursor) -> Result<()> {
        let mut command = vec![0x19];
        command.extend(cursor.id.to_le_bytes());
        self.command(&command).await?;
        self.command_incomplete = false;
        Ok(())
    }

    pub async fn start_binlog(&mut self, server_id: u32, file: &str, position: u32) -> Result<()> {
        let mut register = Vec::new();
        ComRegisterSlave::new(server_id).serialize(&mut register);
        self.command(&register).await?;
        let packet = timeout(self.packets.timeout, self.packets.read())
            .await
            .context("MySQL replication registration timed out")??;
        check_error(&packet)?;
        validate_ok(&packet)?;
        self.command_incomplete = false;
        let mut request = Vec::new();
        ComBinlogDump::new(server_id)
            .with_filename(file.as_bytes())
            .with_pos(position)
            .serialize(&mut request);
        self.command(&request).await?;
        self.replication = true;
        Ok(())
    }

    pub async fn event(&mut self) -> Result<Bytes> {
        anyhow::ensure!(self.replication, "MySQL binlog has not started");
        let packet = self.packets.read().await?;
        check_error(&packet)?;
        anyhow::ensure!(
            packet.first() == Some(&0) && packet.len() > 1,
            "MySQL binlog stream ended or returned an invalid packet"
        );
        Ok(packet.slice(1..))
    }

    pub async fn close(mut self) -> Result<()> {
        timeout(self.packets.timeout, self.packets.stream.shutdown())
            .await
            .context("MySQL socket shutdown timed out")?
            .context("MySQL socket shutdown failed")
    }
}

fn greeting(packet: &[u8]) -> Result<HandshakePacket<'_>> {
    let version_end = packet
        .get(1..)
        .context("missing MySQL protocol version")?
        .iter()
        .position(|byte| *byte == 0)
        .context("unterminated MySQL server version")?
        + 1;
    // Validate before mysql_common's signed length arithmetic and nonce padding.
    anyhow::ensure!(
        packet.first() == Some(&10)
            && packet.get(version_end + 21) == Some(&21)
            && packet.get(version_end + 44) == Some(&0)
            && packet
                .get(version_end + 45..)
                .is_some_and(|plugin| plugin.len() > 1
                    && plugin.last() == Some(&0)
                    && !plugin[..plugin.len() - 1].contains(&0)),
        "invalid MySQL authentication scramble length or terminator"
    );
    let mut buffer = ParseBuf(packet);
    let hello: HandshakePacket<'_> = buffer.parse(())?;
    anyhow::ensure!(
        buffer.0.is_empty()
            && hello.scramble_1_ref().len() == 8
            && hello.scramble_2_ref().is_some_and(|part| part.len() == 13),
        "invalid MySQL greeting"
    );
    Ok(hello)
}

fn validate_plugin(plugin: &AuthPlugin<'_>, nonce: &[u8]) -> Result<()> {
    anyhow::ensure!(
        matches!(
            plugin,
            AuthPlugin::MysqlNativePassword | AuthPlugin::CachingSha2Password
        ) && nonce.len() == 20,
        "unsupported MySQL authentication plugin or nonce"
    );
    Ok(())
}

pub(super) fn check_error(packet: &[u8]) -> Result<()> {
    if packet.first() != Some(&0xff) {
        return Ok(());
    }
    let mut buffer = ParseBuf(packet);
    let error: ErrPacket<'_> = buffer.parse(CapabilityFlags::CLIENT_PROTOCOL_41)?;
    let ErrPacket::Error(error) = error else {
        anyhow::bail!("unexpected MySQL progress report");
    };
    Err(MySqlServerError {
        code: error.error_code(),
        sql_state: error
            .sql_state_ref()
            .map(|state| state.as_str().to_string()),
        message: std::str::from_utf8(error.message_ref())?.to_string(),
    }
    .into())
}

fn validate_ok(packet: &[u8]) -> Result<()> {
    anyhow::ensure!(packet.first() == Some(&0), "expected MySQL OK response");
    let mut data = &packet[1..];
    length(&mut data)?.context("invalid affected row count")?;
    length(&mut data)?.context("invalid last insert ID")?;
    anyhow::ensure!(data.len() >= 4, "truncated MySQL OK response");
    validate_status(u16::from_le_bytes([data[0], data[1]]))
}

fn validate_eof(packet: &[u8]) -> Result<()> {
    anyhow::ensure!(
        packet.len() == 5 && packet[0] == 0xfe,
        "expected MySQL EOF response"
    );
    validate_status(u16::from_le_bytes([packet[3], packet[4]]))
}

fn validate_status(status: u16) -> Result<()> {
    anyhow::ensure!(
        status & 8 == 0,
        "multiple MySQL result sets are unsupported"
    );
    Ok(())
}

fn account(total: &mut usize, bytes: usize, limit: usize) -> Result<()> {
    anyhow::ensure!(
        bytes <= limit.saturating_sub(*total),
        "MySQL query result exceeds protocol byte limit"
    );
    *total += bytes;
    Ok(())
}

pub(super) fn length(input: &mut &[u8]) -> Result<Option<usize>> {
    let (&tag, rest) = input.split_first().context("truncated MySQL length")?;
    *input = rest;
    let count = match tag {
        0..=250 => return Ok(Some(tag.into())),
        251 => return Ok(None),
        252 => 2,
        253 => 3,
        254 => 8,
        _ => anyhow::bail!("invalid MySQL length tag"),
    };
    anyhow::ensure!(input.len() >= count, "truncated MySQL length");
    let mut bytes = [0; 8];
    bytes[..count].copy_from_slice(&input[..count]);
    *input = &input[count..];
    Ok(Some(u64::from_le_bytes(bytes).try_into()?))
}

#[cfg(test)]
mod tests;
