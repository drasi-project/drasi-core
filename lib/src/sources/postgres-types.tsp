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

/**
 * PostgreSQL Type Mappings and WAL Message Format
 *
 * This file documents the PostgreSQL-specific types used by the Postgres source.
 * It includes:
 * - PostgreSQL data type representations
 * - WAL (Write-Ahead Log) message structures
 * - Type mappings from PostgreSQL OIDs to Drasi ElementValues
 *
 * Source: server-core/src/sources/postgres/types.rs
 *         server-core/src/sources/postgres/decoder.rs
 */

import "@typespec/json-schema";

using TypeSpec.JsonSchema;

namespace Drasi.Sources.Postgres;

/**
 * PostgreSQL value types.
 *
 * Represents the various data types that can be decoded from PostgreSQL's
 * replication stream. These are converted to Drasi ElementValues.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 22-45)
 */
@jsonSchema
@discriminator("type")
union PostgresValue {
  null: PostgresNull,
  bool: PostgresBool,
  int2: PostgresInt2,
  int4: PostgresInt4,
  int8: PostgresInt8,
  float4: PostgresFloat4,
  float8: PostgresFloat8,
  numeric: PostgresNumeric,
  text: PostgresText,
  varchar: PostgresVarchar,
  char: PostgresChar,
  uuid: PostgresUuid,
  timestamp: PostgresTimestamp,
  timestamptz: PostgresTimestampTz,
  date: PostgresDate,
  time: PostgresTime,
  json: PostgresJson,
  jsonb: PostgresJsonb,
  array: PostgresArray,
  composite: PostgresComposite,
  bytea: PostgresBytea
}

@jsonSchema
model PostgresNull {
  type: "null";
}

@jsonSchema
model PostgresBool {
  type: "bool";
  value: boolean;
}

@jsonSchema
model PostgresInt2 {
  type: "int2";
  value: int16;
}

@jsonSchema
model PostgresInt4 {
  type: "int4";
  value: int32;
}

@jsonSchema
model PostgresInt8 {
  type: "int8";
  value: int64;
}

@jsonSchema
model PostgresFloat4 {
  type: "float4";
  value: float32;
}

@jsonSchema
model PostgresFloat8 {
  type: "float8";
  value: float64;
}

@jsonSchema
model PostgresNumeric {
  type: "numeric";
  /** Decimal value as string to preserve precision */
  value: string;
}

@jsonSchema
model PostgresText {
  type: "text";
  value: string;
}

@jsonSchema
model PostgresVarchar {
  type: "varchar";
  value: string;
}

@jsonSchema
model PostgresChar {
  type: "char";
  value: string;
}

@jsonSchema
model PostgresUuid {
  type: "uuid";
  /** UUID in standard format (e.g., "550e8400-e29b-41d4-a716-446655440000") */
  value: string;
}

@jsonSchema
model PostgresTimestamp {
  type: "timestamp";
  /** ISO 8601 timestamp without timezone */
  value: string;
}

@jsonSchema
model PostgresTimestampTz {
  type: "timestamptz";
  /** ISO 8601 timestamp with timezone (RFC 3339) */
  value: string;
}

@jsonSchema
model PostgresDate {
  type: "date";
  /** Date in ISO 8601 format (YYYY-MM-DD) */
  value: string;
}

@jsonSchema
model PostgresTime {
  type: "time";
  /** Time in ISO 8601 format (HH:MM:SS) */
  value: string;
}

@jsonSchema
model PostgresJson {
  type: "json";
  /** JSON value (any valid JSON) */
  value: unknown;
}

@jsonSchema
model PostgresJsonb {
  type: "jsonb";
  /** JSON value (any valid JSON) */
  value: unknown;
}

@jsonSchema
model PostgresArray {
  type: "array";
  /** Array of PostgreSQL values */
  value: PostgresValue[];
}

@jsonSchema
model PostgresComposite {
  type: "composite";
  /** Map of field names to PostgreSQL values */
  value: Record<PostgresValue>;
}

@jsonSchema
model PostgresBytea {
  type: "bytea";
  /** Base64-encoded byte array */
  value: string;
}

/**
 * Column information from PostgreSQL relation.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 47-53)
 */
@jsonSchema
model ColumnInfo {
  /** Column name */
  name: string;

  /** PostgreSQL type OID */
  type_oid: uint32;

  /** Type modifier (e.g., varchar length) */
  type_modifier: int32;

  /** Whether this column is part of the primary key */
  is_key: boolean;
}

/**
 * Replica identity setting for a PostgreSQL table.
 *
 * Determines what information is included in UPDATE and DELETE WAL records.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 64-70)
 */
@jsonSchema
enum ReplicaIdentity {
  /** DEFAULT - only primary key columns in old tuple for UPDATE/DELETE */
  default: "default",

  /** NOTHING - no old tuple data */
  nothing: "nothing",

  /** FULL - all columns in old tuple */
  full: "full",

  /** INDEX - columns of specified unique index */
  index: "index"
}

/**
 * Relation (table) information from PostgreSQL.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 55-62)
 */
@jsonSchema
model RelationInfo {
  /** Relation ID (unique within database) */
  id: uint32;

  /** Schema/namespace name */
  namespace: string;

  /** Table name */
  name: string;

  /** Replica identity setting */
  replica_identity: ReplicaIdentity;

  /** Column definitions */
  columns: ColumnInfo[];
}

/**
 * Transaction information from PostgreSQL WAL.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 72-77)
 */
@jsonSchema
model TransactionInfo {
  /** Transaction ID */
  xid: uint32;

  /** LSN (Log Sequence Number) where transaction committed */
  commit_lsn: uint64;

  /** Timestamp when transaction committed */
  commit_timestamp: string;
}

/**
 * WAL (Write-Ahead Log) message types.
 *
 * These are the different types of logical replication messages that
 * PostgreSQL sends via the pgoutput plugin.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 79-100)
 */
@jsonSchema
@discriminator("type")
union WalMessage {
  /** Transaction begin */
  begin: WalBegin,

  /** Transaction commit */
  commit: WalCommit,

  /** Relation definition */
  relation: WalRelation,

  /** Insert operation */
  insert: WalInsert,

  /** Update operation */
  update: WalUpdate,

  /** Delete operation */
  delete: WalDelete,

  /** Truncate operation */
  truncate: WalTruncate
}

@jsonSchema
model WalBegin {
  type: "begin";
  transaction: TransactionInfo;
}

@jsonSchema
model WalCommit {
  type: "commit";
  transaction: TransactionInfo;
}

@jsonSchema
model WalRelation {
  type: "relation";
  relation: RelationInfo;
}

@jsonSchema
model WalInsert {
  type: "insert";
  /** Relation ID this insert belongs to */
  relation_id: uint32;
  /** New tuple values */
  tuple: PostgresValue[];
}

@jsonSchema
model WalUpdate {
  type: "update";
  /** Relation ID this update belongs to */
  relation_id: uint32;
  /** Old tuple values (may be null depending on replica identity) */
  old_tuple?: PostgresValue[];
  /** New tuple values */
  new_tuple: PostgresValue[];
}

@jsonSchema
model WalDelete {
  type: "delete";
  /** Relation ID this delete belongs to */
  relation_id: uint32;
  /** Old tuple values (may be partial depending on replica identity) */
  old_tuple: PostgresValue[];
}

@jsonSchema
model WalTruncate {
  type: "truncate";
  /** Relation IDs being truncated */
  relation_ids: uint32[];
}

/**
 * Replication slot information.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 102-108)
 */
@jsonSchema
model ReplicationSlotInfo {
  /** Replication slot name */
  slot_name: string;

  /** LSN where slot became consistent */
  consistent_point: string;

  /** Snapshot name for initial data load */
  snapshot_name?: string;

  /** Output plugin name (typically "pgoutput") */
  output_plugin: string;
}

/**
 * Standby status update message.
 *
 * Sent periodically to PostgreSQL to acknowledge received WAL data.
 *
 * Based on: server-core/src/sources/postgres/types.rs (lines 110-116)
 */
@jsonSchema
model StandbyStatusUpdate {
  /** LSN of last received WAL data */
  write_lsn: uint64;

  /** LSN of last flushed WAL data */
  flush_lsn: uint64;

  /** LSN of last applied WAL data */
  apply_lsn: uint64;

  /** Whether a reply is requested */
  reply_requested: boolean;
}

/**
 * Common PostgreSQL type OIDs (Object Identifiers).
 *
 * These are standard OIDs from PostgreSQL's pg_type catalog.
 * Custom types will have different OIDs per database.
 */
@jsonSchema
enum CommonTypeOid {
  /** Boolean type (16) */
  bool: 16,

  /** 2-byte integer (21) */
  int2: 21,

  /** 4-byte integer (23) */
  int4: 23,

  /** 8-byte integer (20) */
  int8: 20,

  /** Single precision float (700) */
  float4: 700,

  /** Double precision float (701) */
  float8: 701,

  /** Variable precision numeric (1700) */
  numeric: 1700,

  /** Variable-length text (25) */
  text: 25,

  /** Variable-length character (1043) */
  varchar: 1043,

  /** Fixed-length character (1042) */
  bpchar: 1042,

  /** UUID type (2950) */
  uuid: 2950,

  /** Timestamp without timezone (1114) */
  timestamp: 1114,

  /** Timestamp with timezone (1184) */
  timestamptz: 1184,

  /** Date (1082) */
  date: 1082,

  /** Time without timezone (1083) */
  time: 1083,

  /** JSON type (114) */
  json: 114,

  /** JSONB type (3802) */
  jsonb: 3802,

  /** Bytea (binary) type (17) */
  bytea: 17
}

/**
 * Example PostgreSQL to Drasi type conversion:
 *
 * PostgreSQL Value -> Drasi ElementValue:
 * - NULL -> ElementValue::Null
 * - BOOL -> ElementValue::Bool
 * - INT2/INT4/INT8 -> ElementValue::Integer
 * - FLOAT4/FLOAT8 -> ElementValue::Float
 * - NUMERIC -> ElementValue::String (to preserve precision)
 * - TEXT/VARCHAR/CHAR -> ElementValue::String
 * - UUID -> ElementValue::String (formatted as "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")
 * - TIMESTAMP/TIMESTAMPTZ/DATE/TIME -> ElementValue::String (ISO 8601 format)
 * - JSON/JSONB -> Converted to ElementValue recursively
 * - Array -> ElementValue::List
 * - Composite -> ElementValue::Object
 * - BYTEA -> ElementValue::String (base64 encoded)
 */
