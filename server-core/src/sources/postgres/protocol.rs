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
use bytes::{BufMut, BytesMut};
use std::collections::HashMap;

pub const PROTOCOL_VERSION: u32 = 0x00030000; // 3.0

#[derive(Debug, Clone)]
pub enum FrontendMessage {
    StartupMessage(StartupMessage),
    PasswordMessage(String),
    Query(String),
    Parse {
        name: String,
        query: String,
        param_types: Vec<u32>,
    },
    Bind {
        portal: String,
        statement: String,
        formats: Vec<i16>,
        values: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    Execute {
        portal: String,
        max_rows: i32,
    },
    Sync,
    Terminate,
    CopyData(Vec<u8>),
    CopyDone,
    CopyFail(String),
    // SASL authentication
    SASLInitialResponse {
        mechanism: String,
        data: Vec<u8>,
    },
    SASLResponse(Vec<u8>),
    // Replication specific
    StandbyStatusUpdate {
        write_lsn: u64,
        flush_lsn: u64,
        apply_lsn: u64,
        timestamp: i64,
        reply: u8,
    },
}

#[derive(Debug, Clone)]
pub struct StartupMessage {
    pub parameters: HashMap<String, String>,
}

impl StartupMessage {
    pub fn new_replication(database: &str, user: &str) -> Self {
        let mut parameters = HashMap::new();
        parameters.insert("user".to_string(), user.to_string());
        parameters.insert("database".to_string(), database.to_string());
        parameters.insert("replication".to_string(), "database".to_string());
        Self { parameters }
    }
}

impl FrontendMessage {
    pub fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        match self {
            FrontendMessage::StartupMessage(msg) => {
                let mut msg_buf = BytesMut::new();
                msg_buf.put_u32(PROTOCOL_VERSION);

                for (key, value) in &msg.parameters {
                    msg_buf.put_slice(key.as_bytes());
                    msg_buf.put_u8(0);
                    msg_buf.put_slice(value.as_bytes());
                    msg_buf.put_u8(0);
                }
                msg_buf.put_u8(0); // Final terminator

                // Length includes itself
                buf.put_u32((msg_buf.len() + 4) as u32);
                buf.put_slice(&msg_buf);
            }

            FrontendMessage::PasswordMessage(password) => {
                buf.put_u8(b'p');
                buf.put_u32((4 + password.len() + 1) as u32);
                buf.put_slice(password.as_bytes());
                buf.put_u8(0);
            }

            FrontendMessage::Query(query) => {
                buf.put_u8(b'Q');
                buf.put_u32((4 + query.len() + 1) as u32);
                buf.put_slice(query.as_bytes());
                buf.put_u8(0);
            }

            FrontendMessage::Terminate => {
                buf.put_u8(b'X');
                buf.put_u32(4);
            }

            FrontendMessage::CopyData(data) => {
                buf.put_u8(b'd');
                buf.put_u32((4 + data.len()) as u32);
                buf.put_slice(data);
            }

            FrontendMessage::CopyDone => {
                buf.put_u8(b'c');
                buf.put_u32(4);
            }

            FrontendMessage::CopyFail(msg) => {
                buf.put_u8(b'f');
                buf.put_u32((4 + msg.len() + 1) as u32);
                buf.put_slice(msg.as_bytes());
                buf.put_u8(0);
            }

            FrontendMessage::SASLInitialResponse { mechanism, data } => {
                buf.put_u8(b'p');
                let mut msg_buf = BytesMut::new();
                msg_buf.put_slice(mechanism.as_bytes());
                msg_buf.put_u8(0);
                msg_buf.put_u32(data.len() as u32);
                msg_buf.put_slice(data);
                buf.put_u32((4 + msg_buf.len()) as u32);
                buf.put_slice(&msg_buf);
            }

            FrontendMessage::SASLResponse(data) => {
                buf.put_u8(b'p');
                buf.put_u32((4 + data.len()) as u32);
                buf.put_slice(data);
            }

            FrontendMessage::StandbyStatusUpdate {
                write_lsn,
                flush_lsn,
                apply_lsn,
                timestamp,
                reply,
            } => {
                let mut data = BytesMut::new();
                data.put_u8(b'r'); // Standby status update
                data.put_u64(*write_lsn);
                data.put_u64(*flush_lsn);
                data.put_u64(*apply_lsn);
                data.put_i64(*timestamp);
                data.put_u8(*reply);

                buf.put_u8(b'd'); // CopyData
                buf.put_u32((4 + data.len()) as u32);
                buf.put_slice(&data);
            }

            _ => return Err(anyhow!("Unsupported message type for encoding")),
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum BackendMessage {
    Authentication(AuthenticationMessage),
    BackendKeyData {
        process_id: i32,
        secret_key: i32,
    },
    BindComplete,
    CloseComplete,
    CommandComplete(String),
    CopyBothResponse,
    CopyData(Vec<u8>),
    CopyDone,
    CopyInResponse,
    CopyOutResponse,
    DataRow(Vec<Option<Vec<u8>>>),
    EmptyQueryResponse,
    ErrorResponse(ErrorResponse),
    NoData,
    NoticeResponse(NoticeResponse),
    NotificationResponse,
    ParameterDescription,
    ParameterStatus {
        name: String,
        value: String,
    },
    ParseComplete,
    PortalSuspended,
    ReadyForQuery(TransactionStatus),
    RowDescription(Vec<FieldDescription>),
    // Replication specific
    PrimaryKeepaliveMessage {
        wal_end: u64,
        timestamp: i64,
        reply: u8,
    },
}

#[derive(Debug)]
pub enum AuthenticationMessage {
    Ok,
    KerberosV5,
    CleartextPassword,
    MD5Password([u8; 4]),
    SCMCredential,
    GSS,
    GSSContinue(Vec<u8>),
    SSPI,
    SASL(Vec<String>),
    SASLContinue(Vec<u8>),
    SASLFinal(Vec<u8>),
}

#[derive(Debug)]
pub struct ErrorResponse {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<i32>,
    pub internal_position: Option<i32>,
    pub internal_query: Option<String>,
    pub where_: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub datatype: Option<String>,
    pub constraint: Option<String>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub routine: Option<String>,
}

#[derive(Debug)]
pub struct NoticeResponse {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum TransactionStatus {
    Idle,
    Transaction,
    Failed,
}

#[derive(Debug)]
pub struct FieldDescription {
    pub name: String,
    pub table_oid: u32,
    pub column_id: i16,
    pub type_oid: u32,
    pub type_size: i16,
    pub type_modifier: i32,
    pub format: i16,
}

pub fn parse_backend_message(msg_type: u8, body: &[u8]) -> Result<BackendMessage> {
    match msg_type {
        b'R' => parse_authentication(body),
        b'K' => parse_backend_key_data(body),
        b'Z' => parse_ready_for_query(body),
        b'S' => parse_parameter_status(body),
        b'E' => parse_error_response(body),
        b'N' => parse_notice_response(body),
        b'C' => parse_command_complete(body),
        b'T' => parse_row_description(body),
        b'D' => parse_data_row(body),
        b'W' => parse_copy_both_response(body),
        b'd' => Ok(BackendMessage::CopyData(body.to_vec())),
        b'c' => Ok(BackendMessage::CopyDone),
        b'1' => Ok(BackendMessage::ParseComplete),
        b'2' => Ok(BackendMessage::BindComplete),
        b'3' => Ok(BackendMessage::CloseComplete),
        b'n' => Ok(BackendMessage::NoData),
        b'I' => Ok(BackendMessage::EmptyQueryResponse),
        b's' => Ok(BackendMessage::PortalSuspended),
        _ => Err(anyhow!(
            "Unknown backend message type: {}",
            msg_type as char
        )),
    }
}

fn parse_authentication(body: &[u8]) -> Result<BackendMessage> {
    if body.len() < 4 {
        return Err(anyhow!("Authentication message too short"));
    }

    let auth_type = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let auth = match auth_type {
        0 => AuthenticationMessage::Ok,
        3 => AuthenticationMessage::CleartextPassword,
        5 => {
            if body.len() < 8 {
                return Err(anyhow!("MD5 authentication message too short"));
            }
            let mut salt = [0u8; 4];
            salt.copy_from_slice(&body[4..8]);
            AuthenticationMessage::MD5Password(salt)
        }
        10 => {
            if body.len() > 4 {
                let mechanisms = parse_sasl_mechanisms(&body[4..])?;
                AuthenticationMessage::SASL(mechanisms)
            } else {
                AuthenticationMessage::SASL(vec![])
            }
        }
        11 => {
            if body.len() > 4 {
                AuthenticationMessage::SASLContinue(body[4..].to_vec())
            } else {
                AuthenticationMessage::SASLContinue(vec![])
            }
        }
        12 => {
            if body.len() > 4 {
                AuthenticationMessage::SASLFinal(body[4..].to_vec())
            } else {
                AuthenticationMessage::SASLFinal(vec![])
            }
        }
        _ => return Err(anyhow!("Unsupported authentication type: {}", auth_type)),
    };

    Ok(BackendMessage::Authentication(auth))
}

fn parse_sasl_mechanisms(body: &[u8]) -> Result<Vec<String>> {
    let mut mechanisms = Vec::new();
    let mut pos = 0;

    while pos < body.len() {
        let end = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| anyhow!("Unterminated SASL mechanism"))?;

        if end == 0 {
            break; // Double null terminator
        }

        mechanisms.push(String::from_utf8_lossy(&body[pos..pos + end]).to_string());
        pos += end + 1;
    }

    Ok(mechanisms)
}

fn parse_backend_key_data(body: &[u8]) -> Result<BackendMessage> {
    if body.len() != 8 {
        return Err(anyhow!("BackendKeyData message wrong size"));
    }

    let process_id = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let secret_key = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);

    Ok(BackendMessage::BackendKeyData {
        process_id,
        secret_key,
    })
}

fn parse_ready_for_query(body: &[u8]) -> Result<BackendMessage> {
    if body.len() != 1 {
        return Err(anyhow!("ReadyForQuery message wrong size"));
    }

    let status = match body[0] {
        b'I' => TransactionStatus::Idle,
        b'T' => TransactionStatus::Transaction,
        b'E' => TransactionStatus::Failed,
        _ => return Err(anyhow!("Unknown transaction status: {}", body[0])),
    };

    Ok(BackendMessage::ReadyForQuery(status))
}

fn parse_parameter_status(body: &[u8]) -> Result<BackendMessage> {
    let name_end = body
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| anyhow!("Unterminated parameter name"))?;

    let name = String::from_utf8_lossy(&body[..name_end]).to_string();

    let value_start = name_end + 1;
    let value_end = body[value_start..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| anyhow!("Unterminated parameter value"))?;

    let value = String::from_utf8_lossy(&body[value_start..value_start + value_end]).to_string();

    Ok(BackendMessage::ParameterStatus { name, value })
}

fn parse_error_response(body: &[u8]) -> Result<BackendMessage> {
    let fields = parse_notice_fields(body)?;
    Ok(BackendMessage::ErrorResponse(ErrorResponse {
        severity: fields.get("S").cloned().unwrap_or_default(),
        code: fields.get("C").cloned().unwrap_or_default(),
        message: fields.get("M").cloned().unwrap_or_default(),
        detail: fields.get("D").cloned(),
        hint: fields.get("H").cloned(),
        position: fields.get("P").and_then(|s| s.parse().ok()),
        internal_position: fields.get("p").and_then(|s| s.parse().ok()),
        internal_query: fields.get("q").cloned(),
        where_: fields.get("W").cloned(),
        schema: fields.get("s").cloned(),
        table: fields.get("t").cloned(),
        column: fields.get("c").cloned(),
        datatype: fields.get("d").cloned(),
        constraint: fields.get("n").cloned(),
        file: fields.get("F").cloned(),
        line: fields.get("L").and_then(|s| s.parse().ok()),
        routine: fields.get("R").cloned(),
    }))
}

fn parse_notice_response(body: &[u8]) -> Result<BackendMessage> {
    let fields = parse_notice_fields(body)?;
    Ok(BackendMessage::NoticeResponse(NoticeResponse {
        severity: fields.get("S").cloned().unwrap_or_default(),
        code: fields.get("C").cloned().unwrap_or_default(),
        message: fields.get("M").cloned().unwrap_or_default(),
        detail: fields.get("D").cloned(),
        hint: fields.get("H").cloned(),
    }))
}

fn parse_notice_fields(body: &[u8]) -> Result<HashMap<String, String>> {
    let mut fields = HashMap::new();
    let mut pos = 0;

    while pos < body.len() && body[pos] != 0 {
        let field_type = body[pos] as char;
        pos += 1;

        let end = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| anyhow!("Unterminated field value"))?;

        let value = String::from_utf8_lossy(&body[pos..pos + end]).to_string();
        fields.insert(field_type.to_string(), value);

        pos += end + 1;
    }

    Ok(fields)
}

fn parse_command_complete(body: &[u8]) -> Result<BackendMessage> {
    let end = body
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| anyhow!("Unterminated command tag"))?;

    let tag = String::from_utf8_lossy(&body[..end]).to_string();
    Ok(BackendMessage::CommandComplete(tag))
}

fn parse_row_description(body: &[u8]) -> Result<BackendMessage> {
    let mut pos = 0;
    let field_count = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
    pos += 2;

    let mut fields = Vec::with_capacity(field_count);

    for _ in 0..field_count {
        let name_end = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| anyhow!("Unterminated field name"))?;

        let name = String::from_utf8_lossy(&body[pos..pos + name_end]).to_string();
        pos += name_end + 1;

        if pos + 18 > body.len() {
            return Err(anyhow!("Row description truncated"));
        }

        let table_oid =
            u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;

        let column_id = i16::from_be_bytes([body[pos], body[pos + 1]]);
        pos += 2;

        let type_oid = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;

        let type_size = i16::from_be_bytes([body[pos], body[pos + 1]]);
        pos += 2;

        let type_modifier =
            i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;

        let format = i16::from_be_bytes([body[pos], body[pos + 1]]);
        pos += 2;

        fields.push(FieldDescription {
            name,
            table_oid,
            column_id,
            type_oid,
            type_size,
            type_modifier,
            format,
        });
    }

    Ok(BackendMessage::RowDescription(fields))
}

fn parse_data_row(body: &[u8]) -> Result<BackendMessage> {
    let mut pos = 0;
    let column_count = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
    pos += 2;

    let mut columns = Vec::with_capacity(column_count);

    for _ in 0..column_count {
        if pos + 4 > body.len() {
            return Err(anyhow!("Data row truncated"));
        }

        let length = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;

        if length == -1 {
            columns.push(None);
        } else {
            let length = length as usize;
            if pos + length > body.len() {
                return Err(anyhow!("Data row value truncated"));
            }
            columns.push(Some(body[pos..pos + length].to_vec()));
            pos += length;
        }
    }

    Ok(BackendMessage::DataRow(columns))
}

fn parse_copy_both_response(_body: &[u8]) -> Result<BackendMessage> {
    // Note: PostgreSQL CopyBothResponse includes format codes (overall format + per-column formats),
    // but we don't need to parse them for replication streaming because:
    // 1. Replication protocol uses a known binary format
    // 2. We handle the actual data parsing in the replication-specific message handlers
    // 3. Format codes are primarily useful for COPY operations with variable formats
    //
    // PostgreSQL protocol spec for CopyBothResponse ('W'):
    // - Int32: message length
    // - Int8: overall copy format (0=text, 1=binary)
    // - Int16: number of columns
    // - Int16[]: format code for each column (0=text, 1=binary)
    //
    // If variable format support is needed in the future, parse format codes here.
    Ok(BackendMessage::CopyBothResponse)
}
