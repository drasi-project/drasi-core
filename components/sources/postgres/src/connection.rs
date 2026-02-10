// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{anyhow, Result};
use bytes::{Buf, BytesMut};
use log::{debug, info, trace, warn};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::protocol::{
    parse_backend_message, AuthenticationMessage, BackendMessage, FrontendMessage, StartupMessage,
    TransactionStatus,
};
use super::scram::ScramClient;
use super::types::{ReplicationSlotInfo, StandbyStatusUpdate};

pub struct ReplicationConnection {
    stream: TcpStream,
    read_buffer: BytesMut,
    write_buffer: BytesMut,
    parameters: HashMap<String, String>,
    process_id: Option<i32>,
    secret_key: Option<i32>,
    transaction_status: TransactionStatus,
    in_copy_mode: bool,
}

impl ReplicationConnection {
    pub async fn connect(
        host: &str,
        port: u16,
        database: &str,
        user: &str,
        password: &str,
    ) -> Result<Self> {
        info!("Connecting to PostgreSQL at {host}:{port}");

        let stream = TcpStream::connect((host, port)).await?;
        stream.set_nodelay(true)?;

        let mut conn = Self {
            stream,
            read_buffer: BytesMut::with_capacity(8192),
            write_buffer: BytesMut::with_capacity(8192),
            parameters: HashMap::new(),
            process_id: None,
            secret_key: None,
            transaction_status: TransactionStatus::Idle,
            in_copy_mode: false,
        };

        conn.startup_replication(database, user, password).await?;

        Ok(conn)
    }

    async fn startup_replication(
        &mut self,
        database: &str,
        user: &str,
        password: &str,
    ) -> Result<()> {
        debug!("Starting replication protocol handshake");

        // Send startup message
        let startup = StartupMessage::new_replication(database, user);
        self.send_message(FrontendMessage::StartupMessage(startup))
            .await?;

        // Handle authentication
        loop {
            let msg = self.read_message().await?;
            match msg {
                BackendMessage::Authentication(auth) => {
                    match auth {
                        AuthenticationMessage::Ok => {
                            debug!("Authentication successful");
                            break;
                        }
                        AuthenticationMessage::CleartextPassword => {
                            debug!("Server requested cleartext password");
                            self.send_message(FrontendMessage::PasswordMessage(
                                password.to_string(),
                            ))
                            .await?;
                        }
                        AuthenticationMessage::MD5Password(_) => {
                            return Err(anyhow!(
                                "MD5 authentication is not supported (insecure). \
                                 Please configure PostgreSQL to use scram-sha-256 in pg_hba.conf"
                            ));
                        }
                        AuthenticationMessage::SASL(mechanisms) => {
                            if mechanisms.contains(&"SCRAM-SHA-256".to_string()) {
                                debug!("Server requested SCRAM-SHA-256 authentication");
                                let mut scram_client = ScramClient::new(user, password);

                                // Send SASLInitialResponse
                                let client_first = scram_client.client_first_message();
                                self.send_sasl_initial_response("SCRAM-SHA-256", &client_first)
                                    .await?;

                                // Continue SASL exchange
                                loop {
                                    let sasl_msg = self.read_message().await?;
                                    match sasl_msg {
                                        BackendMessage::Authentication(
                                            AuthenticationMessage::SASLContinue(data),
                                        ) => {
                                            let server_first = String::from_utf8_lossy(&data);
                                            scram_client
                                                .process_server_first_message(&server_first)?;

                                            let client_final =
                                                scram_client.client_final_message()?;
                                            self.send_sasl_response(&client_final).await?;
                                        }
                                        BackendMessage::Authentication(
                                            AuthenticationMessage::SASLFinal(data),
                                        ) => {
                                            let server_final = String::from_utf8_lossy(&data);
                                            scram_client.verify_server_final(&server_final)?;
                                            debug!("SCRAM-SHA-256 authentication successful");
                                            break;
                                        }
                                        BackendMessage::ErrorResponse(err) => {
                                            return Err(anyhow!(
                                                "SASL authentication failed: {}",
                                                err.message
                                            ));
                                        }
                                        _ => {
                                            warn!("Unexpected message during SASL: {sasl_msg:?}");
                                        }
                                    }
                                }
                            } else {
                                return Err(anyhow!("No supported SASL mechanisms"));
                            }
                        }
                        _ => {
                            return Err(anyhow!("Unsupported authentication method"));
                        }
                    }
                }
                BackendMessage::ErrorResponse(err) => {
                    return Err(anyhow!("Authentication failed: {}", err.message));
                }
                _ => {
                    warn!("Unexpected message during authentication: {msg:?}");
                }
            }
        }

        // Wait for ReadyForQuery
        loop {
            let msg = self.read_message().await?;
            match msg {
                BackendMessage::BackendKeyData {
                    process_id,
                    secret_key,
                } => {
                    self.process_id = Some(process_id);
                    self.secret_key = Some(secret_key);
                    debug!("Received backend key data: pid={process_id}");
                }
                BackendMessage::ParameterStatus { name, value } => {
                    debug!("Parameter: {name} = {value}");
                    self.parameters.insert(name, value);
                }
                BackendMessage::ReadyForQuery(status) => {
                    self.transaction_status = status;
                    debug!("Connection ready, status: {status:?}");
                    break;
                }
                BackendMessage::ErrorResponse(err) => {
                    return Err(anyhow!("Startup failed: {}", err.message));
                }
                BackendMessage::NoticeResponse(notice) => {
                    info!("Notice: {}", notice.message);
                }
                _ => {
                    warn!("Unexpected message during startup: {msg:?}");
                }
            }
        }

        Ok(())
    }

    pub async fn identify_system(&mut self) -> Result<HashMap<String, String>> {
        debug!("Sending IDENTIFY_SYSTEM command");

        self.send_message(FrontendMessage::Query("IDENTIFY_SYSTEM".to_string()))
            .await?;

        let mut system_info = HashMap::new();

        loop {
            let msg = self.read_message().await?;
            match msg {
                BackendMessage::RowDescription(_) => {
                    // Skip row description
                }
                BackendMessage::DataRow(row) => {
                    // Parse system identification
                    if row.len() >= 4 {
                        if let Some(Some(systemid)) = row.first() {
                            system_info.insert(
                                "systemid".to_string(),
                                String::from_utf8_lossy(systemid).to_string(),
                            );
                        }
                        if let Some(Some(timeline)) = row.get(1) {
                            system_info.insert(
                                "timeline".to_string(),
                                String::from_utf8_lossy(timeline).to_string(),
                            );
                        }
                        if let Some(Some(xlogpos)) = row.get(2) {
                            system_info.insert(
                                "xlogpos".to_string(),
                                String::from_utf8_lossy(xlogpos).to_string(),
                            );
                        }
                        if let Some(Some(dbname)) = row.get(3) {
                            system_info.insert(
                                "dbname".to_string(),
                                String::from_utf8_lossy(dbname).to_string(),
                            );
                        }
                    }
                }
                BackendMessage::CommandComplete(_) => {
                    // Command completed
                }
                BackendMessage::ReadyForQuery(status) => {
                    self.transaction_status = status;
                    break;
                }
                BackendMessage::ErrorResponse(err) => {
                    return Err(anyhow!("IDENTIFY_SYSTEM failed: {}", err.message));
                }
                _ => {
                    warn!("Unexpected message during IDENTIFY_SYSTEM: {msg:?}");
                }
            }
        }

        Ok(system_info)
    }

    pub async fn create_replication_slot(
        &mut self,
        slot_name: &str,
        temporary: bool,
    ) -> Result<ReplicationSlotInfo> {
        debug!("Creating replication slot: {slot_name}");

        let query = if temporary {
            format!("CREATE_REPLICATION_SLOT {slot_name} TEMPORARY LOGICAL pgoutput")
        } else {
            format!("CREATE_REPLICATION_SLOT {slot_name} LOGICAL pgoutput")
        };

        self.send_message(FrontendMessage::Query(query)).await?;

        let mut slot_info = ReplicationSlotInfo {
            slot_name: slot_name.to_string(),
            consistent_point: String::new(),
            snapshot_name: None,
            output_plugin: "pgoutput".to_string(),
        };

        loop {
            let msg = self.read_message().await?;
            match msg {
                BackendMessage::RowDescription(_) => {
                    // Skip row description
                }
                BackendMessage::DataRow(row) => {
                    // Parse slot creation result
                    if row.len() >= 4 {
                        if let Some(Some(consistent_point)) = row.get(1) {
                            slot_info.consistent_point =
                                String::from_utf8_lossy(consistent_point).to_string();
                        }
                        if let Some(Some(snapshot_name)) = row.get(2) {
                            slot_info.snapshot_name =
                                Some(String::from_utf8_lossy(snapshot_name).to_string());
                        }
                    }
                }
                BackendMessage::CommandComplete(_) => {
                    // Command completed
                }
                BackendMessage::ReadyForQuery(status) => {
                    self.transaction_status = status;
                    break;
                }
                BackendMessage::ErrorResponse(err) => {
                    if err.message.contains("already exists") {
                        debug!("Replication slot already exists: {slot_name}");
                        // Drain the ReadyForQuery that PostgreSQL sends after ErrorResponse
                        loop {
                            let drain_msg = self.read_message().await?;
                            if let BackendMessage::ReadyForQuery(status) = drain_msg {
                                self.transaction_status = status;
                                break;
                            }
                        }
                        return self.get_replication_slot_info(slot_name).await;
                    }
                    return Err(anyhow!("CREATE_REPLICATION_SLOT failed: {}", err.message));
                }
                _ => {
                    warn!("Unexpected message during CREATE_REPLICATION_SLOT: {msg:?}");
                }
            }
        }

        Ok(slot_info)
    }

    pub async fn get_replication_slot_info(
        &mut self,
        slot_name: &str,
    ) -> Result<ReplicationSlotInfo> {
        debug!("Querying existing replication slot: {slot_name}");

        let slot_name_escaped = slot_name.replace('\'', "''");
        let query = format!(
            "SELECT slot_name, confirmed_flush_lsn, restart_lsn, plugin FROM pg_replication_slots WHERE slot_name = '{slot_name_escaped}'"
        );

        self.send_message(FrontendMessage::Query(query)).await?;

        let mut slot_info = ReplicationSlotInfo {
            slot_name: slot_name.to_string(),
            consistent_point: "0/0".to_string(),
            snapshot_name: None,
            output_plugin: "pgoutput".to_string(),
        };
        let mut found_row = false;

        loop {
            let msg = self.read_message().await?;
            match msg {
                BackendMessage::RowDescription(_) => {
                    // Skip row description
                }
                BackendMessage::DataRow(row) => {
                    found_row = true;
                    if row.len() >= 4 {
                        if let Some(Some(confirmed_flush_lsn)) = row.get(1) {
                            let lsn = String::from_utf8_lossy(confirmed_flush_lsn).to_string();
                            if !lsn.is_empty() {
                                slot_info.consistent_point = lsn;
                            }
                        }
                        if slot_info.consistent_point == "0/0" {
                            if let Some(Some(restart_lsn)) = row.get(2) {
                                let lsn = String::from_utf8_lossy(restart_lsn).to_string();
                                if !lsn.is_empty() {
                                    slot_info.consistent_point = lsn;
                                }
                            }
                        }
                        if let Some(Some(plugin)) = row.get(3) {
                            slot_info.output_plugin = String::from_utf8_lossy(plugin).to_string();
                        }
                    }
                }
                BackendMessage::CommandComplete(_) => {
                    // Command completed
                }
                BackendMessage::ReadyForQuery(status) => {
                    self.transaction_status = status;
                    break;
                }
                BackendMessage::ErrorResponse(err) => {
                    return Err(anyhow!("Failed to query replication slot: {}", err.message));
                }
                _ => {
                    warn!("Unexpected message during slot query: {msg:?}");
                }
            }
        }

        if !found_row {
            return Err(anyhow!("Replication slot not found: {slot_name}"));
        }

        info!(
            "Using existing replication slot: {slot_name} at LSN {}",
            slot_info.consistent_point
        );
        Ok(slot_info)
    }

    pub async fn start_replication(
        &mut self,
        slot_name: &str,
        start_lsn: Option<u64>,
        options: HashMap<String, String>,
    ) -> Result<()> {
        debug!("Starting replication from slot: {slot_name}");

        let mut query = format!("START_REPLICATION SLOT {slot_name} LOGICAL");

        if let Some(lsn) = start_lsn {
            query.push_str(&format!(" {}", format_lsn(lsn)));
        } else {
            query.push_str(" 0/0");
        }

        if !options.is_empty() {
            query.push_str(" (");
            let opts: Vec<String> = options.iter().map(|(k, v)| format!("{k} '{v}'")).collect();
            query.push_str(&opts.join(", "));
            query.push(')');
        }

        self.send_message(FrontendMessage::Query(query)).await?;

        // Wait for CopyBothResponse
        loop {
            let msg = self.read_message().await?;
            match msg {
                BackendMessage::CopyBothResponse => {
                    debug!("Entered COPY BOTH mode for replication");
                    self.in_copy_mode = true;
                    break;
                }
                BackendMessage::ErrorResponse(err) => {
                    return Err(anyhow!("START_REPLICATION failed: {}", err.message));
                }
                BackendMessage::ReadyForQuery(_) => {
                    // This is normal - PostgreSQL sends ReadyForQuery before entering COPY mode
                    debug!("Received ReadyForQuery before entering COPY mode");
                }
                _ => {
                    debug!("Message during START_REPLICATION: {msg:?}");
                }
            }
        }

        Ok(())
    }

    pub async fn read_replication_message(&mut self) -> Result<BackendMessage> {
        if !self.in_copy_mode {
            return Err(anyhow!("Not in COPY mode"));
        }

        self.read_message().await
    }

    pub async fn send_standby_status(&mut self, status: StandbyStatusUpdate) -> Result<()> {
        if !self.in_copy_mode {
            return Err(anyhow!("Not in COPY mode"));
        }

        let timestamp = chrono::Utc::now().timestamp_micros() - 946684800000000; // PostgreSQL epoch

        self.send_message(FrontendMessage::StandbyStatusUpdate {
            write_lsn: status.write_lsn,
            flush_lsn: status.flush_lsn,
            apply_lsn: status.apply_lsn,
            timestamp,
            reply: if status.reply_requested { 1 } else { 0 },
        })
        .await
    }

    async fn send_message(&mut self, msg: FrontendMessage) -> Result<()> {
        self.write_buffer.clear();
        msg.encode(&mut self.write_buffer)?;

        self.stream.write_all(&self.write_buffer).await?;
        self.stream.flush().await?;

        trace!("Sent message: {msg:?}");
        Ok(())
    }

    async fn send_sasl_initial_response(&mut self, mechanism: &str, response: &str) -> Result<()> {
        self.send_message(FrontendMessage::SASLInitialResponse {
            mechanism: mechanism.to_string(),
            data: response.as_bytes().to_vec(),
        })
        .await
    }

    async fn send_sasl_response(&mut self, response: &str) -> Result<()> {
        self.send_message(FrontendMessage::SASLResponse(response.as_bytes().to_vec()))
            .await
    }

    async fn read_message(&mut self) -> Result<BackendMessage> {
        loop {
            // Try to parse a message from the buffer
            if let Some(msg) = self.try_parse_message()? {
                trace!("Received message: {msg:?}");
                return Ok(msg);
            }

            // Read more data
            let mut temp_buf = vec![0u8; 4096];
            let n = self.stream.read(&mut temp_buf).await?;
            if n == 0 {
                return Err(anyhow!("Connection closed by server"));
            }

            self.read_buffer.extend_from_slice(&temp_buf[..n]);
        }
    }

    fn try_parse_message(&mut self) -> Result<Option<BackendMessage>> {
        if self.read_buffer.len() < 5 {
            return Ok(None); // Need at least type + length
        }

        let msg_type = self.read_buffer[0];
        let length = u32::from_be_bytes([
            self.read_buffer[1],
            self.read_buffer[2],
            self.read_buffer[3],
            self.read_buffer[4],
        ]) as usize;

        if length < 4 {
            return Err(anyhow!("Invalid message length: {length}"));
        }

        let total_length = 1 + length; // Type byte + length (includes self)

        if self.read_buffer.len() < total_length {
            return Ok(None); // Need more data
        }

        // Extract message
        let body = self.read_buffer[5..total_length].to_vec();
        self.read_buffer.advance(total_length);

        // Parse message
        let msg = parse_backend_message(msg_type, &body)?;
        Ok(Some(msg))
    }

    pub async fn close(mut self) -> Result<()> {
        if self.in_copy_mode {
            let _ = self.send_message(FrontendMessage::CopyDone).await;
        }
        let _ = self.send_message(FrontendMessage::Terminate).await;
        let _ = self.stream.shutdown().await;
        Ok(())
    }
}

fn format_lsn(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn & 0xFFFFFFFF)
}

#[allow(dead_code)]
fn parse_lsn(lsn_str: &str) -> Result<u64> {
    let parts: Vec<&str> = lsn_str.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow!("Invalid LSN format: {lsn_str}"));
    }

    let high = u64::from_str_radix(parts[0], 16)?;
    let low = u64::from_str_radix(parts[1], 16)?;

    Ok((high << 32) | low)
}
