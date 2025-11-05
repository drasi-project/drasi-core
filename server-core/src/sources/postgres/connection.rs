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
        info!("Connecting to PostgreSQL at {}:{}", host, port);

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
                        AuthenticationMessage::MD5Password(salt) => {
                            debug!("Server requested MD5 password");
                            let md5_password = compute_md5_password(user, password, &salt);
                            self.send_message(FrontendMessage::PasswordMessage(md5_password))
                                .await?;
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
                                            warn!("Unexpected message during SASL: {:?}", sasl_msg);
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
                    warn!("Unexpected message during authentication: {:?}", msg);
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
                    debug!("Received backend key data: pid={}", process_id);
                }
                BackendMessage::ParameterStatus { name, value } => {
                    debug!("Parameter: {} = {}", name, value);
                    self.parameters.insert(name, value);
                }
                BackendMessage::ReadyForQuery(status) => {
                    self.transaction_status = status;
                    debug!("Connection ready, status: {:?}", status);
                    break;
                }
                BackendMessage::ErrorResponse(err) => {
                    return Err(anyhow!("Startup failed: {}", err.message));
                }
                BackendMessage::NoticeResponse(notice) => {
                    info!("Notice: {}", notice.message);
                }
                _ => {
                    warn!("Unexpected message during startup: {:?}", msg);
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
                    warn!("Unexpected message during IDENTIFY_SYSTEM: {:?}", msg);
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
        debug!("Creating replication slot: {}", slot_name);

        let query = if temporary {
            format!(
                "CREATE_REPLICATION_SLOT {} TEMPORARY LOGICAL pgoutput",
                slot_name
            )
        } else {
            format!("CREATE_REPLICATION_SLOT {} LOGICAL pgoutput", slot_name)
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
                        debug!("Replication slot already exists: {}", slot_name);
                        // Try to get existing slot info
                        return self.get_replication_slot_info(slot_name).await;
                    }
                    return Err(anyhow!("CREATE_REPLICATION_SLOT failed: {}", err.message));
                }
                _ => {
                    warn!(
                        "Unexpected message during CREATE_REPLICATION_SLOT: {:?}",
                        msg
                    );
                }
            }
        }

        Ok(slot_info)
    }

    pub async fn get_replication_slot_info(
        &mut self,
        slot_name: &str,
    ) -> Result<ReplicationSlotInfo> {
        // For existing slots, we can assume they exist and use default values
        // The actual slot info will be retrieved when we start replication
        debug!("Using existing replication slot: {}", slot_name);

        Ok(ReplicationSlotInfo {
            slot_name: slot_name.to_string(),
            consistent_point: "0/0".to_string(), // Will be updated from START_REPLICATION
            snapshot_name: None,
            output_plugin: "pgoutput".to_string(),
        })
    }

    pub async fn start_replication(
        &mut self,
        slot_name: &str,
        start_lsn: Option<u64>,
        options: HashMap<String, String>,
    ) -> Result<()> {
        debug!("Starting replication from slot: {}", slot_name);

        let mut query = format!("START_REPLICATION SLOT {} LOGICAL", slot_name);

        if let Some(lsn) = start_lsn {
            query.push_str(&format!(" {}", format_lsn(lsn)));
        } else {
            query.push_str(" 0/0");
        }

        if !options.is_empty() {
            query.push_str(" (");
            let opts: Vec<String> = options
                .iter()
                .map(|(k, v)| format!("{} '{}'", k, v))
                .collect();
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
                    debug!("Message during START_REPLICATION: {:?}", msg);
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

        trace!("Sent message: {:?}", msg);
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
                trace!("Received message: {:?}", msg);
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
            return Err(anyhow!("Invalid message length: {}", length));
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

fn compute_md5_password(user: &str, password: &str, salt: &[u8; 4]) -> String {
    use std::fmt::Write;

    // First MD5: password + username
    let mut hasher = md5::Context::new();
    hasher.consume(password.as_bytes());
    hasher.consume(user.as_bytes());
    let pass_user_hash = hasher.compute();

    // Convert to hex string
    let mut hex_hash = String::with_capacity(32);
    for byte in pass_user_hash.iter() {
        // Writing to a String should never fail, but handle gracefully
        let _ = write!(&mut hex_hash, "{:02x}", byte);
    }

    // Second MD5: hex_hash + salt
    let mut hasher = md5::Context::new();
    hasher.consume(hex_hash.as_bytes());
    hasher.consume(salt);
    let final_hash = hasher.compute();

    // Convert to hex string with "md5" prefix
    let mut result = String::from("md5");
    for byte in final_hash.iter() {
        // Writing to a String should never fail, but handle gracefully
        let _ = write!(&mut result, "{:02x}", byte);
    }

    result
}

fn format_lsn(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn & 0xFFFFFFFF)
}

#[allow(dead_code)]
fn parse_lsn(lsn_str: &str) -> Result<u64> {
    let parts: Vec<&str> = lsn_str.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow!("Invalid LSN format: {}", lsn_str));
    }

    let high = u64::from_str_radix(parts[0], 16)?;
    let low = u64::from_str_radix(parts[1], 16)?;

    Ok((high << 32) | low)
}

// Simple MD5 implementation for password hashing
mod md5 {
    pub struct Context {
        state: [u32; 4],
        count: [u32; 2],
        buffer: [u8; 64],
    }

    impl Context {
        pub fn new() -> Self {
            Self {
                state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
                count: [0, 0],
                buffer: [0; 64],
            }
        }

        pub fn consume(&mut self, input: &[u8]) {
            let mut input_idx = 0;
            let length = input.len();

            let index = (self.count[0] >> 3) as usize & 0x3F;

            self.count[0] = self.count[0].wrapping_add((length as u32) << 3);
            if self.count[0] < ((length as u32) << 3) {
                self.count[1] = self.count[1].wrapping_add(1);
            }
            self.count[1] = self.count[1].wrapping_add((length as u32) >> 29);

            let part_len = 64 - index;

            let mut i;
            if length >= part_len {
                self.buffer[index..index + part_len].copy_from_slice(&input[..part_len]);
                self.transform(&self.buffer.clone());

                i = part_len;
                while i + 63 < length {
                    let mut chunk = [0u8; 64];
                    chunk.copy_from_slice(&input[i..i + 64]);
                    self.transform(&chunk);
                    i += 64;
                }

                input_idx = i;
            }

            self.buffer[index..index + length - input_idx].copy_from_slice(&input[input_idx..]);
        }

        pub fn compute(mut self) -> [u8; 16] {
            let bits = [self.count[0], self.count[1]];
            let index = (self.count[0] >> 3) as usize & 0x3f;
            let pad_len = if index < 56 { 56 - index } else { 120 - index };

            let mut padding = vec![0u8; pad_len];
            padding[0] = 0x80;
            self.consume(&padding);

            let mut bits_bytes = [0u8; 8];
            for i in 0..2 {
                bits_bytes[i * 4] = bits[i] as u8;
                bits_bytes[i * 4 + 1] = (bits[i] >> 8) as u8;
                bits_bytes[i * 4 + 2] = (bits[i] >> 16) as u8;
                bits_bytes[i * 4 + 3] = (bits[i] >> 24) as u8;
            }
            self.consume(&bits_bytes);

            let mut result = [0u8; 16];
            for i in 0..4 {
                result[i * 4] = self.state[i] as u8;
                result[i * 4 + 1] = (self.state[i] >> 8) as u8;
                result[i * 4 + 2] = (self.state[i] >> 16) as u8;
                result[i * 4 + 3] = (self.state[i] >> 24) as u8;
            }

            result
        }

        fn transform(&mut self, block: &[u8; 64]) {
            let mut a = self.state[0];
            let mut b = self.state[1];
            let mut c = self.state[2];
            let mut d = self.state[3];

            let mut x = [0u32; 16];
            for i in 0..16 {
                x[i] = u32::from_le_bytes([
                    block[i * 4],
                    block[i * 4 + 1],
                    block[i * 4 + 2],
                    block[i * 4 + 3],
                ]);
            }

            // Round 1
            macro_rules! ff {
                ($a:expr, $b:expr, $c:expr, $d:expr, $x:expr, $s:expr, $ac:expr) => {
                    $a = $a
                        .wrapping_add((($b & $c) | (!$b & $d)).wrapping_add($x).wrapping_add($ac));
                    $a = $a.rotate_left($s);
                    $a = $a.wrapping_add($b);
                };
            }

            ff!(a, b, c, d, x[0], 7, 0xd76aa478);
            ff!(d, a, b, c, x[1], 12, 0xe8c7b756);
            ff!(c, d, a, b, x[2], 17, 0x242070db);
            ff!(b, c, d, a, x[3], 22, 0xc1bdceee);
            ff!(a, b, c, d, x[4], 7, 0xf57c0faf);
            ff!(d, a, b, c, x[5], 12, 0x4787c62a);
            ff!(c, d, a, b, x[6], 17, 0xa8304613);
            ff!(b, c, d, a, x[7], 22, 0xfd469501);
            ff!(a, b, c, d, x[8], 7, 0x698098d8);
            ff!(d, a, b, c, x[9], 12, 0x8b44f7af);
            ff!(c, d, a, b, x[10], 17, 0xffff5bb1);
            ff!(b, c, d, a, x[11], 22, 0x895cd7be);
            ff!(a, b, c, d, x[12], 7, 0x6b901122);
            ff!(d, a, b, c, x[13], 12, 0xfd987193);
            ff!(c, d, a, b, x[14], 17, 0xa679438e);
            ff!(b, c, d, a, x[15], 22, 0x49b40821);

            // Round 2
            macro_rules! gg {
                ($a:expr, $b:expr, $c:expr, $d:expr, $x:expr, $s:expr, $ac:expr) => {
                    $a = $a
                        .wrapping_add((($b & $d) | ($c & !$d)).wrapping_add($x).wrapping_add($ac));
                    $a = $a.rotate_left($s);
                    $a = $a.wrapping_add($b);
                };
            }

            gg!(a, b, c, d, x[1], 5, 0xf61e2562);
            gg!(d, a, b, c, x[6], 9, 0xc040b340);
            gg!(c, d, a, b, x[11], 14, 0x265e5a51);
            gg!(b, c, d, a, x[0], 20, 0xe9b6c7aa);
            gg!(a, b, c, d, x[5], 5, 0xd62f105d);
            gg!(d, a, b, c, x[10], 9, 0x02441453);
            gg!(c, d, a, b, x[15], 14, 0xd8a1e681);
            gg!(b, c, d, a, x[4], 20, 0xe7d3fbc8);
            gg!(a, b, c, d, x[9], 5, 0x21e1cde6);
            gg!(d, a, b, c, x[14], 9, 0xc33707d6);
            gg!(c, d, a, b, x[3], 14, 0xf4d50d87);
            gg!(b, c, d, a, x[8], 20, 0x455a14ed);
            gg!(a, b, c, d, x[13], 5, 0xa9e3e905);
            gg!(d, a, b, c, x[2], 9, 0xfcefa3f8);
            gg!(c, d, a, b, x[7], 14, 0x676f02d9);
            gg!(b, c, d, a, x[12], 20, 0x8d2a4c8a);

            // Round 3
            macro_rules! hh {
                ($a:expr, $b:expr, $c:expr, $d:expr, $x:expr, $s:expr, $ac:expr) => {
                    $a = $a.wrapping_add(($b ^ $c ^ $d).wrapping_add($x).wrapping_add($ac));
                    $a = $a.rotate_left($s);
                    $a = $a.wrapping_add($b);
                };
            }

            hh!(a, b, c, d, x[5], 4, 0xfffa3942);
            hh!(d, a, b, c, x[8], 11, 0x8771f681);
            hh!(c, d, a, b, x[11], 16, 0x6d9d6122);
            hh!(b, c, d, a, x[14], 23, 0xfde5380c);
            hh!(a, b, c, d, x[1], 4, 0xa4beea44);
            hh!(d, a, b, c, x[4], 11, 0x4bdecfa9);
            hh!(c, d, a, b, x[7], 16, 0xf6bb4b60);
            hh!(b, c, d, a, x[10], 23, 0xbebfbc70);
            hh!(a, b, c, d, x[13], 4, 0x289b7ec6);
            hh!(d, a, b, c, x[0], 11, 0xeaa127fa);
            hh!(c, d, a, b, x[3], 16, 0xd4ef3085);
            hh!(b, c, d, a, x[6], 23, 0x04881d05);
            hh!(a, b, c, d, x[9], 4, 0xd9d4d039);
            hh!(d, a, b, c, x[12], 11, 0xe6db99e5);
            hh!(c, d, a, b, x[15], 16, 0x1fa27cf8);
            hh!(b, c, d, a, x[2], 23, 0xc4ac5665);

            // Round 4
            macro_rules! ii {
                ($a:expr, $b:expr, $c:expr, $d:expr, $x:expr, $s:expr, $ac:expr) => {
                    $a = $a.wrapping_add(($c ^ ($b | !$d)).wrapping_add($x).wrapping_add($ac));
                    $a = $a.rotate_left($s);
                    $a = $a.wrapping_add($b);
                };
            }

            ii!(a, b, c, d, x[0], 6, 0xf4292244);
            ii!(d, a, b, c, x[7], 10, 0x432aff97);
            ii!(c, d, a, b, x[14], 15, 0xab9423a7);
            ii!(b, c, d, a, x[5], 21, 0xfc93a039);
            ii!(a, b, c, d, x[12], 6, 0x655b59c3);
            ii!(d, a, b, c, x[3], 10, 0x8f0ccc92);
            ii!(c, d, a, b, x[10], 15, 0xffeff47d);
            ii!(b, c, d, a, x[1], 21, 0x85845dd1);
            ii!(a, b, c, d, x[8], 6, 0x6fa87e4f);
            ii!(d, a, b, c, x[15], 10, 0xfe2ce6e0);
            ii!(c, d, a, b, x[6], 15, 0xa3014314);
            ii!(b, c, d, a, x[13], 21, 0x4e0811a1);
            ii!(a, b, c, d, x[4], 6, 0xf7537e82);
            ii!(d, a, b, c, x[11], 10, 0xbd3af235);
            ii!(c, d, a, b, x[2], 15, 0x2ad7d2bb);
            ii!(b, c, d, a, x[9], 21, 0xeb86d391);

            self.state[0] = self.state[0].wrapping_add(a);
            self.state[1] = self.state[1].wrapping_add(b);
            self.state[2] = self.state[2].wrapping_add(c);
            self.state[3] = self.state[3].wrapping_add(d);
        }
    }
}
