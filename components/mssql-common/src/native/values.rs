// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQL Server value mapping, shared by the CDC source and the
//! snapshot providers.
//!
//! Each captured column gets a [`ColumnKind`] from its catalog type. The
//! kind decides the SELECT expression (conversions run in SQL, so decimals
//! keep their exact text and spatial or XML values arrive as text), the
//! decoded value and the published schema type. No column type rejects a
//! table: a type with no mapping is streamed as NULL with schema type
//! `None`.

use super::identifiers::quoted;
use drasi_core_types::{models::ElementValue, schema::PropertyType};
use std::sync::Arc;

/// Largest decimal precision emitted as a float; every decimal with at most
/// 15 significant digits round-trips through `f64` exactly.
pub const MAX_FLOAT_DECIMAL_PRECISION: u8 = 15;

/// How a column is selected, decoded and described.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ColumnKind {
    /// tinyint, smallint, int, bigint.
    Integer,
    /// float, real, smallmoney and decimal/numeric with precision ≤ 15.
    Float,
    /// money and decimal/numeric with precision > 15, as exact decimal text.
    Decimal,
    /// bit.
    Boolean,
    /// datetime, datetime2, smalldatetime.
    LocalDateTime,
    /// datetimeoffset.
    ZonedDateTime,
    /// date, as ISO text.
    Date,
    /// time, as text.
    Time,
    /// char, nchar, varchar, nvarchar, text, ntext.
    Text,
    /// uniqueidentifier, as lowercase hyphenated text.
    Guid,
    /// binary, varbinary, image, timestamp/rowversion, as `0x` hex text.
    Binary,
    /// json (SQL Server 2025), decoded into a structured value.
    Json,
    /// xml, geography, geometry, hierarchyid, sql_variant: converted to text
    /// in SQL; schema type `None`.
    Converted,
    /// Any other type (CLR types, vector, …): NULL, schema type `None`.
    Unmapped,
}

impl ColumnKind {
    /// The kind of a column of base type `type_name`.
    pub fn of(type_name: &str, precision: u8) -> Self {
        match type_name.to_ascii_lowercase().as_str() {
            "tinyint" | "smallint" | "int" | "bigint" => Self::Integer,
            "float" | "real" | "smallmoney" => Self::Float,
            "decimal" | "numeric" if precision <= MAX_FLOAT_DECIMAL_PRECISION => Self::Float,
            "decimal" | "numeric" | "money" => Self::Decimal,
            "bit" => Self::Boolean,
            "datetime" | "datetime2" | "smalldatetime" => Self::LocalDateTime,
            "datetimeoffset" => Self::ZonedDateTime,
            "date" => Self::Date,
            "time" => Self::Time,
            "char" | "nchar" | "varchar" | "nvarchar" | "text" | "ntext" | "sysname" => Self::Text,
            "uniqueidentifier" => Self::Guid,
            "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => Self::Binary,
            "json" => Self::Json,
            "xml" | "geography" | "geometry" | "hierarchyid" | "sql_variant" => Self::Converted,
            _ => Self::Unmapped,
        }
    }

    /// The published schema type.
    pub fn property_type(self) -> Option<PropertyType> {
        match self {
            Self::Integer => Some(PropertyType::Integer),
            Self::Float => Some(PropertyType::Float),
            Self::Boolean => Some(PropertyType::Boolean),
            Self::LocalDateTime | Self::ZonedDateTime => Some(PropertyType::Timestamp),
            Self::Decimal | Self::Date | Self::Time | Self::Text | Self::Guid | Self::Binary => {
                Some(PropertyType::String)
            }
            Self::Json => Some(PropertyType::Json),
            Self::Converted | Self::Unmapped => None,
        }
    }

    /// Whether values of this kind can form an element key.
    pub fn keyable(self) -> bool {
        !matches!(self, Self::Json | Self::Converted | Self::Unmapped)
    }

    /// The SELECT expression for `column` of base type `type_name`, aliased
    /// to the column name.
    pub fn select(self, column: &str, type_name: &str) -> String {
        let name = quoted(column);
        let expression = match (self, type_name.to_ascii_lowercase().as_str()) {
            (Self::Float, "decimal" | "numeric" | "smallmoney") => {
                format!("CAST({name} AS float)")
            }
            // Style 2 keeps all four money decimals.
            (Self::Decimal, "money") => format!("CONVERT(nvarchar(64), {name}, 2)"),
            (Self::Decimal, _) => format!("CONVERT(nvarchar(64), {name})"),
            (Self::Json, _) => format!("CONVERT(nvarchar(max), {name})"),
            (Self::Converted, "geography" | "geometry") => format!("{name}.STAsText()"),
            (Self::Converted, "hierarchyid") => format!("{name}.ToString()"),
            (Self::Converted, "sql_variant") => format!("CONVERT(nvarchar(4000), {name})"),
            (Self::Converted, _) => format!("CONVERT(nvarchar(max), {name})"),
            (Self::Unmapped, _) => "CAST(NULL AS int)".to_owned(),
            _ => name.clone(),
        };
        format!("{expression} AS {name}")
    }
}

/// Decodes JSON text into a structured value; text that is not JSON stays a
/// string, so a value is never dropped.
pub fn json_value(text: &str) -> ElementValue {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => ElementValue::from(&value),
        Err(_) => ElementValue::String(Arc::from(text)),
    }
}

/// `0x`-prefixed lowercase hex, the binary text form shared with the legacy
/// source.
pub fn binary_text(bytes: &[u8]) -> ElementValue {
    ElementValue::String(Arc::from(format!("0x{}", hex::encode(bytes))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_type_has_a_kind_and_none_rejects_a_table() {
        for (name, precision, kind, schema) in [
            ("int", 10, ColumnKind::Integer, Some(PropertyType::Integer)),
            ("decimal", 15, ColumnKind::Float, Some(PropertyType::Float)),
            (
                "decimal",
                38,
                ColumnKind::Decimal,
                Some(PropertyType::String),
            ),
            ("money", 19, ColumnKind::Decimal, Some(PropertyType::String)),
            (
                "smallmoney",
                10,
                ColumnKind::Float,
                Some(PropertyType::Float),
            ),
            ("bit", 1, ColumnKind::Boolean, Some(PropertyType::Boolean)),
            (
                "datetime2",
                27,
                ColumnKind::LocalDateTime,
                Some(PropertyType::Timestamp),
            ),
            (
                "datetimeoffset",
                34,
                ColumnKind::ZonedDateTime,
                Some(PropertyType::Timestamp),
            ),
            ("nvarchar", 0, ColumnKind::Text, Some(PropertyType::String)),
            (
                "uniqueidentifier",
                0,
                ColumnKind::Guid,
                Some(PropertyType::String),
            ),
            (
                "rowversion",
                0,
                ColumnKind::Binary,
                Some(PropertyType::String),
            ),
            ("json", 0, ColumnKind::Json, Some(PropertyType::Json)),
            ("xml", 0, ColumnKind::Converted, None),
            ("geography", 0, ColumnKind::Converted, None),
            ("geometry", 0, ColumnKind::Converted, None),
            ("hierarchyid", 0, ColumnKind::Converted, None),
            ("sql_variant", 0, ColumnKind::Converted, None),
            ("vector", 0, ColumnKind::Unmapped, None),
            ("SomeClrType", 0, ColumnKind::Unmapped, None),
        ] {
            let actual = ColumnKind::of(name, precision);
            assert_eq!(actual, kind, "{name}");
            assert_eq!(actual.property_type(), schema, "{name}");
        }
    }

    #[test]
    fn conversions_run_in_sql_and_keep_the_column_name() {
        assert_eq!(
            ColumnKind::Decimal.select("Total", "decimal"),
            "CONVERT(nvarchar(64), [Total]) AS [Total]"
        );
        assert_eq!(
            ColumnKind::Decimal.select("Price", "money"),
            "CONVERT(nvarchar(64), [Price], 2) AS [Price]"
        );
        assert_eq!(
            ColumnKind::Float.select("Rate", "numeric"),
            "CAST([Rate] AS float) AS [Rate]"
        );
        assert_eq!(
            ColumnKind::Converted.select("Shape", "geography"),
            "[Shape].STAsText() AS [Shape]"
        );
        assert_eq!(
            ColumnKind::Converted.select("Node", "hierarchyid"),
            "[Node].ToString() AS [Node]"
        );
        assert_eq!(
            ColumnKind::Converted.select("Doc", "xml"),
            "CONVERT(nvarchar(max), [Doc]) AS [Doc]"
        );
        assert_eq!(
            ColumnKind::Unmapped.select("Odd]Name", "vector"),
            "CAST(NULL AS int) AS [Odd]]Name]"
        );
        assert_eq!(ColumnKind::Integer.select("Id", "int"), "[Id] AS [Id]");
    }

    #[test]
    fn json_and_binary_values_never_drop_data() {
        let ElementValue::Object(object) = json_value(r#"{"a":[1,2]}"#) else {
            panic!("object expected");
        };
        assert!(object.get("a").is_some());
        assert_eq!(
            json_value("not json"),
            ElementValue::String(Arc::from("not json"))
        );
        assert_eq!(
            binary_text(&[0xab, 1]),
            ElementValue::String(Arc::from("0xab01"))
        );
        assert!(!ColumnKind::Json.keyable() && ColumnKind::Guid.keyable());
    }
}
