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

//! PostgreSQL type OID constants and information_schema helpers.

pub const BOOL: u32 = 16;
pub const BYTEA: u32 = 17;
pub const NAME: u32 = 19;
pub const INT8: u32 = 20;
pub const INT2: u32 = 21;
pub const INT4: u32 = 23;
pub const TEXT: u32 = 25;
pub const JSON: u32 = 114;
pub const FLOAT4: u32 = 700;
pub const FLOAT8: u32 = 701;
pub const CHAR: u32 = 1042;
pub const VARCHAR: u32 = 1043;
pub const DATE: u32 = 1082;
pub const TIME: u32 = 1083;
pub const TIMESTAMP: u32 = 1114;
pub const TIMESTAMPTZ: u32 = 1184;
pub const NUMERIC: u32 = 1700;
pub const UUID: u32 = 2950;
pub const JSONB: u32 = 3802;

// Common 1-D array OIDs
pub const BOOL_ARRAY: u32 = 1000;
pub const BYTEA_ARRAY: u32 = 1001;
pub const INT2_ARRAY: u32 = 1005;
pub const INT4_ARRAY: u32 = 1007;
pub const TEXT_ARRAY: u32 = 1009;
pub const VARCHAR_ARRAY: u32 = 1015;
pub const INT8_ARRAY: u32 = 1016;
pub const FLOAT4_ARRAY: u32 = 1021;
pub const FLOAT8_ARRAY: u32 = 1022;
pub const NUMERIC_ARRAY: u32 = 1231;
pub const UUID_ARRAY: u32 = 2951;
pub const JSON_ARRAY: u32 = 199;
pub const JSONB_ARRAY: u32 = 3807;
pub const TIMESTAMP_ARRAY: u32 = 1115;
pub const TIMESTAMPTZ_ARRAY: u32 = 1185;
pub const DATE_ARRAY: u32 = 1182;
pub const TIME_ARRAY: u32 = 1183;

/// Map an `information_schema.columns.data_type` / `udt_name` pair to a type OID.
/// Prefer querying `pg_attribute.atttypid` when possible; this is a fallback.
pub fn oid_from_information_schema(data_type: &str, udt_name: Option<&str>) -> u32 {
    // Arrays report data_type = ARRAY and the element type in udt_name (e.g. `_int4`).
    if data_type.eq_ignore_ascii_case("ARRAY") {
        if let Some(udt) = udt_name {
            return match udt {
                "_bool" => BOOL_ARRAY,
                "_bytea" => BYTEA_ARRAY,
                "_int2" => INT2_ARRAY,
                "_int4" => INT4_ARRAY,
                "_int8" => INT8_ARRAY,
                "_text" => TEXT_ARRAY,
                "_varchar" => VARCHAR_ARRAY,
                "_float4" => FLOAT4_ARRAY,
                "_float8" => FLOAT8_ARRAY,
                "_numeric" => NUMERIC_ARRAY,
                "_uuid" => UUID_ARRAY,
                "_json" => JSON_ARRAY,
                "_jsonb" => JSONB_ARRAY,
                "_timestamp" => TIMESTAMP_ARRAY,
                "_timestamptz" => TIMESTAMPTZ_ARRAY,
                "_date" => DATE_ARRAY,
                "_time" => TIME_ARRAY,
                _ => TEXT_ARRAY,
            };
        }
        return TEXT_ARRAY;
    }

    match data_type {
        "boolean" => BOOL,
        "bytea" => BYTEA,
        "name" => NAME,
        "bigint" => INT8,
        "smallint" => INT2,
        "integer" => INT4,
        "text" => TEXT,
        "json" => JSON,
        "real" => FLOAT4,
        "double precision" => FLOAT8,
        "character" => CHAR,
        "character varying" => VARCHAR,
        "date" => DATE,
        "time without time zone" | "time with time zone" => TIME,
        "timestamp without time zone" => TIMESTAMP,
        "timestamp with time zone" => TIMESTAMPTZ,
        "numeric" | "decimal" => NUMERIC,
        "uuid" => UUID,
        "jsonb" => JSONB,
        _ => {
            // Fall back to udt_name for domains / aliases
            match udt_name.unwrap_or("") {
                "bool" => BOOL,
                "bytea" => BYTEA,
                "int2" => INT2,
                "int4" => INT4,
                "int8" => INT8,
                "float4" => FLOAT4,
                "float8" => FLOAT8,
                "numeric" => NUMERIC,
                "text" => TEXT,
                "varchar" => VARCHAR,
                "bpchar" => CHAR,
                "uuid" => UUID,
                "json" => JSON,
                "jsonb" => JSONB,
                "date" => DATE,
                "time" | "timetz" => TIME,
                "timestamp" => TIMESTAMP,
                "timestamptz" => TIMESTAMPTZ,
                _ => TEXT,
            }
        }
    }
}

/// Returns the element OID for a known array OID, if any.
pub fn array_element_oid(array_oid: u32) -> Option<u32> {
    match array_oid {
        BOOL_ARRAY => Some(BOOL),
        BYTEA_ARRAY => Some(BYTEA),
        INT2_ARRAY => Some(INT2),
        INT4_ARRAY => Some(INT4),
        INT8_ARRAY => Some(INT8),
        TEXT_ARRAY => Some(TEXT),
        VARCHAR_ARRAY => Some(VARCHAR),
        FLOAT4_ARRAY => Some(FLOAT4),
        FLOAT8_ARRAY => Some(FLOAT8),
        NUMERIC_ARRAY => Some(NUMERIC),
        UUID_ARRAY => Some(UUID),
        JSON_ARRAY => Some(JSON),
        JSONB_ARRAY => Some(JSONB),
        TIMESTAMP_ARRAY => Some(TIMESTAMP),
        TIMESTAMPTZ_ARRAY => Some(TIMESTAMPTZ),
        DATE_ARRAY => Some(DATE),
        TIME_ARRAY => Some(TIME),
        _ => None,
    }
}

/// Returns true if the OID is a known array type we can decode.
pub fn is_array_oid(oid: u32) -> bool {
    array_element_oid(oid).is_some()
}
