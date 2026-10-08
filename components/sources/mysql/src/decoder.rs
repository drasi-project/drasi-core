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

//! Decodes MySQL binlog row events into Drasi SourceChange events.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use mysql_async::Value;
use mysql_common::binlog::events::{OptionalMetadataField, TableMapEvent};
use mysql_common::binlog::row::BinlogRow;
use mysql_common::binlog::value::BinlogValue;
use mysql_common::constants::ColumnType;
use ordered_float::OrderedFloat;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};

use drasi_mysql_common::{
    canonicalize_json_text, enum_label, format_datetime, format_time, format_timestamp_epoch,
    format_value_for_key, parse_timestamp_epoch_text, set_labels, TableKeyConfig,
};

pub struct MySqlDecoder {
    source_id: String,
    table_keys: HashMap<String, Vec<String>>,
}

/// Per-column conversion context derived from a TableMapEvent.
struct ColumnContext {
    col_type: Option<ColumnType>,
    /// Fractional-second precision for temporal types, when known.
    fsp: Option<u8>,
    /// ENUM member labels (declaration order), when this column is ENUM.
    enum_labels: Option<Vec<String>>,
    /// SET member labels (declaration order), when this column is SET.
    set_labels: Option<Vec<String>>,
}

impl MySqlDecoder {
    pub fn new(source_id: impl Into<String>, table_keys: &[TableKeyConfig]) -> Self {
        let mut map = HashMap::new();
        for key in table_keys {
            map.insert(key.table.clone(), key.key_columns.clone());
        }
        Self {
            source_id: source_id.into(),
            table_keys: map,
        }
    }

    pub fn decode_insert(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
    ) -> Result<SourceChange> {
        let (element, _) = self.row_to_element(table, row, None)?;
        Ok(SourceChange::Insert { element })
    }

    pub fn decode_update(
        &self,
        table: &TableMapEvent<'_>,
        before: &BinlogRow,
        after: &BinlogRow,
    ) -> Result<SourceChange> {
        let (element, _) = self.row_to_element(table, after, Some(before))?;
        Ok(SourceChange::Update { element })
    }

    pub fn decode_delete(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
    ) -> Result<SourceChange> {
        let (_, metadata) = self.row_to_element(table, row, None)?;
        Ok(SourceChange::Delete { metadata })
    }

    pub(crate) fn native_element(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
        keys: &[String],
        timestamp: u64,
        max_bytes: usize,
    ) -> Result<Element> {
        let database = std::str::from_utf8(table.database_name_raw())?;
        let name = std::str::from_utf8(table.table_name_raw())?;
        let contexts = strict_column_contexts(table, row.len())?;
        let mut properties = ElementPropertyMap::new();
        let mut parts = Vec::new();
        for (index, column) in row.columns_ref().iter().enumerate() {
            let name = std::str::from_utf8(column.name_ref())?;
            anyhow::ensure!(!name.is_empty(), "native MySQL requires full column names");
            let value = row
                .as_ref(index)
                .context("native MySQL requires complete row images")?;
            let binary = column.character_set() == 63;
            let mut value = strict_value(
                value,
                &contexts[index],
                binary,
                column
                    .flags()
                    .contains(mysql_common::constants::ColumnFlags::UNSIGNED_FLAG),
                max_bytes,
            )?;
            if binary && contexts[index].col_type == Some(ColumnType::MYSQL_TYPE_STRING) {
                if let ElementValue::List(bytes) = &mut value {
                    let metadata = table
                        .get_column_metadata(index)
                        .context("missing BINARY width")?;
                    let [kind, width] = metadata else {
                        anyhow::bail!("invalid BINARY metadata");
                    };
                    anyhow::ensure!(
                        *kind == ColumnType::MYSQL_TYPE_STRING as u8
                            && bytes.len() <= usize::from(*width),
                        "invalid MySQL BINARY width"
                    );
                    // The binlog omits trailing zero padding from fixed-width BINARY values.
                    bytes.resize(usize::from(*width), ElementValue::Integer(0));
                }
            }
            if keys.iter().any(|key| key == name) {
                parts.push((name.to_string(), value.clone()));
            }
            properties.insert(name, value);
        }
        anyhow::ensure!(
            parts.len() == keys.len(),
            "native MySQL key columns are missing"
        );
        let identity = drasi_mysql_common::keys::transaction_element_id(database, name, &parts)?;
        Ok(Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(&self.source_id, &identity),
                labels: Arc::from([Arc::from(name)]),
                effective_from: timestamp,
            },
            properties,
        })
    }

    pub(crate) fn native_snapshot_element(
        &self,
        database: &str,
        table: &str,
        columns: &[mysql_common::packets::Column],
        values: Vec<Value>,
        key_columns: &[String],
        max_bytes: usize,
    ) -> Result<Element> {
        use mysql_common::constants::ColumnFlags;
        anyhow::ensure!(columns.len() == values.len(), "incomplete snapshot row");
        let mut properties = ElementPropertyMap::new();
        let mut keys = Vec::new();
        for (column, value) in columns.iter().zip(values) {
            let name = std::str::from_utf8(column.name_ref())?;
            let context = ColumnContext {
                col_type: Some(
                    if column
                        .flags()
                        .intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG)
                    {
                        ColumnType::MYSQL_TYPE_VARCHAR
                    } else {
                        column.column_type()
                    },
                ),
                fsp: Some(column.decimals()),
                enum_labels: None,
                set_labels: None,
            };
            let value = strict_value(
                &BinlogValue::Value(value),
                &context,
                column.character_set() == 63,
                column.flags().contains(ColumnFlags::UNSIGNED_FLAG),
                max_bytes,
            )?;
            if key_columns.iter().any(|key| key == name) {
                keys.push((name.to_string(), value.clone()));
            }
            properties.insert(name, value);
        }
        anyhow::ensure!(
            keys.len() == key_columns.len(),
            "snapshot key columns are missing"
        );
        let identity = drasi_mysql_common::keys::transaction_element_id(database, table, &keys)?;
        Ok(Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(&self.source_id, &identity),
                labels: Arc::from([Arc::from(table)]),
                effective_from: Utc::now().timestamp_millis().try_into()?,
            },
            properties,
        })
    }

    fn row_to_element(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
        fallback_row: Option<&BinlogRow>,
    ) -> Result<(Element, ElementMetadata)> {
        let table_name = table.table_name().into_owned();
        let label = table_name.clone();

        let mut properties = ElementPropertyMap::new();
        let mut key_parts: Vec<String> = Vec::new();
        let configured_keys = self.table_keys.get(table_name.as_str());
        let column_names = self.extract_column_names(row);
        let column_contexts = build_column_contexts(table, row.len());

        let fallback_key = if configured_keys.is_none() && !column_names.is_empty() {
            column_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case("id"))
                .cloned()
        } else {
            None
        };

        for idx in 0..row.len() {
            let column_key = column_names
                .get(idx)
                .cloned()
                .unwrap_or_else(|| format!("col_{idx}"));
            let ctx = column_contexts.get(idx);
            let value = self.value_at(row, fallback_row, idx, ctx)?;

            if let Some(keys) = configured_keys {
                if keys.contains(&column_key) {
                    key_parts.push(format_value_for_key(&value));
                }
            } else if fallback_key
                .as_ref()
                .is_some_and(|fallback| fallback == &column_key)
            {
                key_parts.push(format_value_for_key(&value));
            }

            properties.insert(&column_key, value);
        }

        if key_parts.is_empty() {
            anyhow::bail!(
                "Cannot construct a deterministic element ID for table '{table_name}': \
                 no key columns configured and no 'id' column found. \
                 Configure key_columns for this table."
            );
        }

        let element_id = format!("{}:{}", table_name, key_parts.join("_"));
        let metadata = ElementMetadata {
            reference: ElementReference::new(&self.source_id, &element_id),
            labels: Arc::from(vec![Arc::from(label)]),
            effective_from: Utc::now().timestamp_millis() as u64,
        };
        let element = Element::Node {
            metadata: metadata.clone(),
            properties,
        };
        Ok((element, metadata))
    }

    fn value_at(
        &self,
        row: &BinlogRow,
        fallback_row: Option<&BinlogRow>,
        idx: usize,
        ctx: Option<&ColumnContext>,
    ) -> Result<ElementValue> {
        let value = row
            .as_ref(idx)
            .or_else(|| fallback_row.and_then(|fallback| fallback.as_ref(idx)));
        match value {
            None => Ok(ElementValue::Null),
            Some(value) => binlog_value_to_element_value(value, ctx),
        }
    }

    fn extract_column_names(&self, row: &BinlogRow) -> Vec<String> {
        row.columns_ref()
            .iter()
            .enumerate()
            .map(|(idx, column)| {
                let name = column.name_str().to_string();
                if name.is_empty() {
                    format!("col_{idx}")
                } else {
                    name
                }
            })
            .collect()
    }
}

fn strict_column_contexts(table: &TableMapEvent<'_>, count: usize) -> Result<Vec<ColumnContext>> {
    for field in table.iter_optional_meta() {
        match field? {
            OptionalMetadataField::EnumStrValue(enums) => {
                for entry in enums.iter_values() {
                    for value in entry?.values() {
                        std::str::from_utf8(value.value_raw())?;
                    }
                }
            }
            OptionalMetadataField::SetStrValue(sets) => {
                for entry in sets.iter_values() {
                    for value in entry?.values() {
                        std::str::from_utf8(value.value_raw())?;
                    }
                }
            }
            _ => {}
        }
    }
    for index in 0..count {
        table
            .get_column_type(index)?
            .context("missing MySQL column type")?;
    }
    Ok(build_column_contexts(table, count))
}

fn strict_value(
    value: &BinlogValue<'_>,
    context: &ColumnContext,
    binary: bool,
    unsigned: bool,
    max_bytes: usize,
) -> Result<ElementValue> {
    match value {
        BinlogValue::Value(Value::Int(value))
            if context.col_type == Some(ColumnType::MYSQL_TYPE_INT24) =>
        {
            anyhow::ensure!(
                (if unsigned { 0 } else { -8_388_608 }..=16_777_215).contains(value),
                "invalid MySQL MEDIUMINT"
            );
            // mysql_common 0.37 does not sign-extend its three-byte integer.
            return Ok(ElementValue::Integer(if !unsigned && *value >= 8_388_608 {
                *value - 16_777_216
            } else {
                *value
            }));
        }
        BinlogValue::JsonDiff(_) => {
            anyhow::bail!("partial MySQL JSON updates are unsupported")
        }
        BinlogValue::Jsonb(value) => {
            let mut budget = max_bytes;
            let json = strict_json(value, 0, &mut budget)?;
            return Ok(ElementValue::String(serde_json::to_string(&json)?.into()));
        }
        BinlogValue::Value(Value::Float(value)) => {
            anyhow::ensure!(value.is_finite(), "non-finite MySQL float")
        }
        BinlogValue::Value(Value::Double(value)) => {
            anyhow::ensure!(value.is_finite(), "non-finite MySQL double")
        }
        BinlogValue::Value(Value::Bytes(bytes)) => {
            if context.col_type == Some(ColumnType::MYSQL_TYPE_JSON) {
                let value: serde_json::Value = serde_json::from_slice(bytes)?;
                let mut budget = max_bytes;
                validate_json_text(&value, 0, &mut budget)?;
                return Ok(ElementValue::String(serde_json::to_string(&value)?.into()));
            }
            if matches!(context.col_type, Some(ColumnType::MYSQL_TYPE_SET)) {
                let labels = context
                    .set_labels
                    .as_ref()
                    .context("missing MySQL SET labels")?;
                anyhow::ensure!(
                    bytes.iter().enumerate().all(|(index, byte)| (0..8)
                        .all(|bit| index * 8 + bit < labels.len() || byte & (1 << bit) == 0)),
                    "MySQL SET references unknown labels"
                );
            } else if (binary || context.col_type == Some(ColumnType::MYSQL_TYPE_BIT))
                && matches!(
                    context.col_type,
                    Some(
                        ColumnType::MYSQL_TYPE_STRING
                            | ColumnType::MYSQL_TYPE_VAR_STRING
                            | ColumnType::MYSQL_TYPE_VARCHAR
                            | ColumnType::MYSQL_TYPE_BLOB
                            | ColumnType::MYSQL_TYPE_TINY_BLOB
                            | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                            | ColumnType::MYSQL_TYPE_LONG_BLOB
                            | ColumnType::MYSQL_TYPE_BIT
                    )
                )
            {
                return Ok(ElementValue::List(
                    bytes
                        .iter()
                        .map(|byte| ElementValue::Integer((*byte).into()))
                        .collect(),
                ));
            } else {
                std::str::from_utf8(bytes)?;
            }
        }
        _ => {}
    }
    if matches!(context.col_type, Some(ColumnType::MYSQL_TYPE_ENUM)) {
        let labels = context
            .enum_labels
            .as_ref()
            .context("missing MySQL ENUM labels")?;
        let ordinal = match value {
            BinlogValue::Value(Value::Int(value)) => u64::try_from(*value)?,
            BinlogValue::Value(Value::UInt(value)) => *value,
            BinlogValue::Value(Value::NULL) => 0,
            _ => anyhow::bail!("invalid MySQL ENUM value"),
        };
        anyhow::ensure!(
            ordinal <= labels.len() as u64,
            "MySQL ENUM references an unknown label"
        );
    }
    binlog_value_to_element_value(value, Some(context))
}

fn validate_json_text(value: &serde_json::Value, depth: usize, budget: &mut usize) -> Result<()> {
    anyhow::ensure!(depth < 100, "MySQL JSON nesting exceeds supported depth");
    json_budget(budget, 64)?;
    match value {
        serde_json::Value::String(value) => json_budget(budget, value.len())?,
        serde_json::Value::Array(values) => {
            for value in values {
                validate_json_text(value, depth + 1, budget)?;
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                json_budget(budget, key.len())?;
                validate_json_text(value, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_budget(budget: &mut usize, bytes: usize) -> Result<()> {
    anyhow::ensure!(bytes <= *budget, "decoded MySQL JSON exceeds byte limit");
    *budget -= bytes;
    Ok(())
}

fn strict_json(
    value: &mysql_common::binlog::jsonb::Value<'_>,
    depth: usize,
    budget: &mut usize,
) -> Result<serde_json::Value> {
    use mysql_common::binlog::jsonb::Value as Json;
    anyhow::ensure!(depth < 100, "MySQL JSON nesting exceeds supported depth");
    json_budget(budget, 64)?;
    Ok(match value {
        Json::Null => serde_json::Value::Null,
        Json::Bool(value) => (*value).into(),
        Json::I16(value) => (*value).into(),
        Json::U16(value) => (*value).into(),
        Json::I32(value) => (*value).into(),
        Json::U32(value) => (*value).into(),
        Json::I64(value) => (*value).into(),
        Json::U64(value) => (*value).into(),
        Json::F64(value) => serde_json::Value::Number(
            serde_json::Number::from_f64(*value).context("non-finite MySQL JSON number")?,
        ),
        Json::String(value) => {
            json_budget(budget, value.str_raw().len())?;
            std::str::from_utf8(value.str_raw())?.into()
        }
        Json::SmallArray(value) => strict_json_array(value, depth, budget)?,
        Json::LargeArray(value) => strict_json_array(value, depth, budget)?,
        Json::SmallObject(value) => strict_json_object(value, depth, budget)?,
        Json::LargeObject(value) => strict_json_object(value, depth, budget)?,
        Json::Opaque(_) => anyhow::bail!("opaque MySQL JSON values are unsupported"),
    })
}

fn strict_json_array<T: mysql_common::binlog::jsonb::StorageFormat>(
    value: &mysql_common::binlog::jsonb::ComplexValue<'_, T, mysql_common::binlog::jsonb::Array>,
    depth: usize,
    budget: &mut usize,
) -> Result<serde_json::Value> {
    let mut result = Vec::new();
    for value in value.iter() {
        result.push(strict_json(&value?, depth + 1, budget)?);
    }
    Ok(result.into())
}

fn strict_json_object<T: mysql_common::binlog::jsonb::StorageFormat>(
    value: &mysql_common::binlog::jsonb::ComplexValue<'_, T, mysql_common::binlog::jsonb::Object>,
    depth: usize,
    budget: &mut usize,
) -> Result<serde_json::Value> {
    let mut result = serde_json::Map::new();
    for entry in value.iter() {
        let (key, value) = entry?;
        json_budget(budget, key.value_raw().len())?;
        let key = std::str::from_utf8(key.value_raw())?.to_string();
        anyhow::ensure!(
            result
                .insert(key, strict_json(&value, depth + 1, budget)?)
                .is_none(),
            "duplicate MySQL JSON object key"
        );
    }
    Ok(result.into())
}

/// Build per-column conversion context from TableMapEvent type/metadata + optional ENUM/SET labels.
fn build_column_contexts(table: &TableMapEvent<'_>, column_count: usize) -> Vec<ColumnContext> {
    // Optional metadata lists ENUM/SET definitions in column order among those types only.
    let mut enum_defs: Vec<Vec<String>> = Vec::new();
    let mut set_defs: Vec<Vec<String>> = Vec::new();

    for meta in table.iter_optional_meta() {
        let Ok(field) = meta else {
            continue;
        };
        match field {
            OptionalMetadataField::EnumStrValue(enums) => {
                for entry in enums.iter_values().flatten() {
                    enum_defs.push(
                        entry
                            .values()
                            .iter()
                            .map(|v| v.value().into_owned())
                            .collect(),
                    );
                }
            }
            OptionalMetadataField::SetStrValue(sets) => {
                for entry in sets.iter_values().flatten() {
                    set_defs.push(
                        entry
                            .values()
                            .iter()
                            .map(|v| v.value().into_owned())
                            .collect(),
                    );
                }
            }
            _ => {}
        }
    }

    let mut enum_idx = 0usize;
    let mut set_idx = 0usize;
    let mut contexts = Vec::with_capacity(column_count);

    for idx in 0..column_count {
        let col_type = table.get_column_type(idx).ok().flatten();
        let fsp = temporal_fsp(table, idx, col_type);

        let mut enum_labels = None;
        let mut set_labels_opt = None;

        if matches!(col_type, Some(ColumnType::MYSQL_TYPE_ENUM)) {
            if let Some(labels) = enum_defs.get(enum_idx) {
                enum_labels = Some(labels.clone());
            }
            enum_idx += 1;
        } else if matches!(col_type, Some(ColumnType::MYSQL_TYPE_SET)) {
            if let Some(labels) = set_defs.get(set_idx) {
                set_labels_opt = Some(labels.clone());
            }
            set_idx += 1;
        }

        contexts.push(ColumnContext {
            col_type,
            fsp,
            enum_labels,
            set_labels: set_labels_opt,
        });
    }

    contexts
}

fn temporal_fsp(
    table: &TableMapEvent<'_>,
    col_idx: usize,
    col_type: Option<ColumnType>,
) -> Option<u8> {
    match col_type {
        Some(
            ColumnType::MYSQL_TYPE_TIMESTAMP2
            | ColumnType::MYSQL_TYPE_DATETIME2
            | ColumnType::MYSQL_TYPE_TIME2,
        ) => table
            .get_column_metadata(col_idx)
            .and_then(|meta| meta.first().copied()),
        // Non-*2 temporal types have no fractional seconds.
        Some(
            ColumnType::MYSQL_TYPE_TIMESTAMP
            | ColumnType::MYSQL_TYPE_DATETIME
            | ColumnType::MYSQL_TYPE_TIME
            | ColumnType::MYSQL_TYPE_DATE
            | ColumnType::MYSQL_TYPE_NEWDATE,
        ) => Some(0),
        _ => None,
    }
}

fn binlog_value_to_element_value(
    value: &BinlogValue<'_>,
    ctx: Option<&ColumnContext>,
) -> Result<ElementValue> {
    match value {
        BinlogValue::Value(value) => Ok(mysql_value_to_element_value(value, ctx)),
        BinlogValue::Jsonb(value) => {
            let json = serde_json::Value::try_from(value.clone())
                .context("Failed to convert MySQL JSONB value to JSON")?;
            Ok(ElementValue::String(Arc::from(serde_json::to_string(
                &json,
            )?)))
        }
        BinlogValue::JsonDiff(diff) => Ok(ElementValue::String(Arc::from(format!("{diff:?}")))),
    }
}

fn mysql_value_to_element_value(value: &Value, ctx: Option<&ColumnContext>) -> ElementValue {
    let col_type = ctx.and_then(|c| c.col_type);
    let fsp = ctx.and_then(|c| c.fsp);

    match value {
        Value::NULL => ElementValue::Null,
        Value::Bytes(bytes) => convert_bytes(bytes, col_type, fsp, ctx),
        Value::Int(val) => convert_int(*val, col_type, fsp, ctx),
        Value::UInt(val) => {
            if matches!(col_type, Some(ColumnType::MYSQL_TYPE_ENUM)) {
                if let Some(labels) = ctx.and_then(|c| c.enum_labels.as_deref()) {
                    if !labels.is_empty() {
                        return ElementValue::String(Arc::from(enum_label(*val, labels)));
                    }
                }
                // No label metadata: preserve the ordinal as an integer.
                return if *val <= i64::MAX as u64 {
                    ElementValue::Integer(*val as i64)
                } else {
                    ElementValue::String(Arc::from(val.to_string()))
                };
            }
            if *val <= i64::MAX as u64 {
                ElementValue::Integer(*val as i64)
            } else {
                ElementValue::String(Arc::from(val.to_string()))
            }
        }
        Value::Float(val) => ElementValue::Float(OrderedFloat(*val as f64)),
        Value::Double(val) => ElementValue::Float(OrderedFloat(*val)),
        Value::Date(y, m, d, h, min, s, micros) => ElementValue::String(Arc::from(
            format_datetime(*y, *m, *d, *h, *min, *s, *micros, fsp),
        )),
        Value::Time(neg, days, hours, minutes, seconds, micros) => ElementValue::String(Arc::from(
            format_time(*neg, *days, *hours, *minutes, *seconds, *micros, fsp),
        )),
    }
}

fn convert_int(
    val: i64,
    col_type: Option<ColumnType>,
    fsp: Option<u8>,
    ctx: Option<&ColumnContext>,
) -> ElementValue {
    match col_type {
        Some(ColumnType::MYSQL_TYPE_ENUM) => {
            if let Some(labels) = ctx.and_then(|c| c.enum_labels.as_deref()) {
                if !labels.is_empty() {
                    if let Ok(ordinal) = u64::try_from(val) {
                        return ElementValue::String(Arc::from(enum_label(ordinal, labels)));
                    }
                }
            }
            // No label metadata (or negative ordinal): preserve prior integer behavior.
            ElementValue::Integer(val)
        }
        Some(ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2) => {
            ElementValue::String(Arc::from(format_timestamp_epoch(val, 0, fsp)))
        }
        // mysql_common maps YEAR wire byte 0 → 1900; bootstrap text is "0000" → 0.
        // 1900 is outside the legal YEAR range, so remap back to 0 for parity.
        Some(ColumnType::MYSQL_TYPE_YEAR) => {
            ElementValue::Integer(if val == 1900 { 0 } else { val })
        }
        _ => ElementValue::Integer(val),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn convert_bytes(
    bytes: &[u8],
    col_type: Option<ColumnType>,
    fsp: Option<u8>,
    ctx: Option<&ColumnContext>,
) -> ElementValue {
    match col_type {
        // YEAR may arrive as text ("2025"/"0000") or as mysql_common's 1900+offset form.
        Some(ColumnType::MYSQL_TYPE_YEAR) => {
            let text = String::from_utf8_lossy(bytes);
            if let Ok(val) = text.parse::<i64>() {
                ElementValue::Integer(if val == 1900 { 0 } else { val })
            } else {
                ElementValue::String(Arc::from(text.into_owned()))
            }
        }
        Some(ColumnType::MYSQL_TYPE_SET) => {
            if let Some(labels) = ctx.and_then(|c| c.set_labels.as_deref()) {
                if !labels.is_empty() {
                    return ElementValue::String(Arc::from(set_labels(bytes, labels)));
                }
            }
            // No label metadata: keep a stable hex representation of the bitmask
            // rather than lossy UTF-8 or an empty string.
            ElementValue::String(Arc::from(format!("0x{}", hex_encode(bytes))))
        }
        Some(ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2) => {
            let text = String::from_utf8_lossy(bytes);
            if let Some((secs, micros)) = parse_timestamp_epoch_text(&text) {
                ElementValue::String(Arc::from(format_timestamp_epoch(secs, micros, fsp)))
            } else {
                // Already a formatted datetime, or unparsable — pass through.
                ElementValue::String(Arc::from(text.into_owned()))
            }
        }
        Some(ColumnType::MYSQL_TYPE_JSON) => {
            let text = String::from_utf8_lossy(bytes);
            ElementValue::String(Arc::from(canonicalize_json_text(&text)))
        }
        _ => ElementValue::String(Arc::from(String::from_utf8_lossy(bytes).into_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::Value;
    use ordered_float::OrderedFloat;

    #[test]
    fn native_values_reject_lossy_text_nonfinite_numbers_and_unbounded_json() -> Result<()> {
        use mysql_common::binlog::jsonb::{JsonbString, Value as Json};
        let string = ctx(ColumnType::MYSQL_TYPE_VARCHAR, None, None, None);
        let bytes = BinlogValue::Value(Value::Bytes(vec![0, 255]));
        assert!(strict_value(&bytes, &string, false, false, 1024).is_err());
        assert_eq!(
            strict_value(&bytes, &string, true, false, 1024)?,
            ElementValue::List(vec![ElementValue::Integer(0), ElementValue::Integer(255)])
        );
        assert!(strict_value(
            &BinlogValue::Value(Value::Double(f64::NAN)),
            &string,
            false,
            false,
            1024
        )
        .is_err());
        assert!(strict_value(
            &BinlogValue::JsonDiff(Vec::new()),
            &string,
            false,
            false,
            1024
        )
        .is_err());
        assert!(strict_json(&Json::String(JsonbString::new(vec![255])), 0, &mut 1024).is_err());
        assert!(strict_json(&Json::F64(f64::INFINITY), 0, &mut 1024).is_err());
        assert!(strict_json(&Json::Null, 100, &mut 1024).is_err());
        assert!(strict_json(
            &Json::String(JsonbString::new(b"four".as_slice())),
            0,
            &mut 67
        )
        .is_err());
        assert_eq!(
            strict_json(
                &Json::String(JsonbString::new(b"four".as_slice())),
                0,
                &mut 68
            )?,
            serde_json::json!("four")
        );
        let json = ctx(ColumnType::MYSQL_TYPE_JSON, None, None, None);
        for (text, limit, valid) in [
            ("\"four\"".to_string(), 67, false),
            ("\"four\"".to_string(), 68, true),
            ("[0,0,0]".to_string(), 255, false),
            ("[0,0,0]".to_string(), 256, true),
            ("{".to_string(), 1024, false),
            (
                format!("{}0{}", "[".repeat(99), "]".repeat(99)),
                10000,
                true,
            ),
            (
                format!("{}0{}", "[".repeat(100), "]".repeat(100)),
                10000,
                false,
            ),
        ] {
            assert_eq!(
                strict_value(
                    &BinlogValue::Value(Value::Bytes(text.into_bytes())),
                    &json,
                    false,
                    false,
                    limit
                )
                .is_ok(),
                valid
            );
        }
        let enumeration = ctx(
            ColumnType::MYSQL_TYPE_ENUM,
            None,
            Some(vec!["one".into()]),
            None,
        );
        assert!(strict_value(
            &BinlogValue::Value(Value::Int(2)),
            &enumeration,
            false,
            false,
            1024
        )
        .is_err());
        let set = ctx(
            ColumnType::MYSQL_TYPE_SET,
            None,
            None,
            Some(vec!["one".into()]),
        );
        assert!(strict_value(
            &BinlogValue::Value(Value::Bytes(vec![2])),
            &set,
            false,
            false,
            1024
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn native_mediumint_values_are_sign_extended_without_changing_unsigned_values() -> Result<()> {
        let context = ctx(ColumnType::MYSQL_TYPE_INT24, None, None, None);
        for (raw, signed) in [
            (0, 0),
            (8_388_607, 8_388_607),
            (8_388_608, -8_388_608),
            (16_777_215, -1),
        ] {
            let value = BinlogValue::Value(Value::Int(raw));
            assert_eq!(
                strict_value(&value, &context, false, false, 1024)?,
                ElementValue::Integer(signed)
            );
            assert_eq!(
                strict_value(&value, &context, false, true, 1024)?,
                ElementValue::Integer(raw)
            );
        }
        let negative = BinlogValue::Value(Value::Int(-8_388_608));
        assert_eq!(
            strict_value(&negative, &context, false, false, 1024)?,
            ElementValue::Integer(-8_388_608)
        );
        assert!(strict_value(&negative, &context, false, true, 1024).is_err());
        assert!(strict_value(
            &BinlogValue::Value(Value::Int(16_777_216)),
            &context,
            false,
            false,
            1024
        )
        .is_err());
        Ok(())
    }

    fn ctx(
        col_type: ColumnType,
        fsp: Option<u8>,
        enum_labels: Option<Vec<String>>,
        set_labels: Option<Vec<String>>,
    ) -> ColumnContext {
        ColumnContext {
            col_type: Some(col_type),
            fsp,
            enum_labels,
            set_labels,
        }
    }

    #[test]
    fn test_null() {
        let v = mysql_value_to_element_value(&Value::NULL, None);
        assert_eq!(v, ElementValue::Null);
    }

    #[test]
    fn test_int() {
        let v = mysql_value_to_element_value(&Value::Int(123_456), None);
        assert_eq!(v, ElementValue::Integer(123_456));
    }

    #[test]
    fn test_uint_overflow() {
        let v = mysql_value_to_element_value(&Value::UInt((i64::MAX as u64) + 1), None);
        assert_eq!(
            v,
            ElementValue::String(Arc::from(((i64::MAX as u64) + 1).to_string()))
        );
    }

    #[test]
    fn test_float() {
        let v = mysql_value_to_element_value(&Value::Float(1.23), None);
        assert_eq!(v, ElementValue::Float(OrderedFloat(f64::from(1.23_f32))));
    }

    #[test]
    fn test_double() {
        let v = mysql_value_to_element_value(&Value::Double(1.23456789), None);
        assert_eq!(v, ElementValue::Float(OrderedFloat(1.23456789)));
    }

    #[test]
    fn test_bytes() {
        let v = mysql_value_to_element_value(&Value::Bytes(b"hello".to_vec()), None);
        assert_eq!(v, ElementValue::String(Arc::from("hello")));
    }

    #[test]
    fn test_date_without_fraction() {
        let c = ctx(ColumnType::MYSQL_TYPE_DATETIME, Some(0), None, None);
        let v = mysql_value_to_element_value(&Value::Date(2024, 6, 15, 13, 45, 30, 0), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("2024-06-15 13:45:30")));
    }

    #[test]
    fn test_date_with_micros() {
        let c = ctx(ColumnType::MYSQL_TYPE_DATETIME2, Some(6), None, None);
        let v =
            mysql_value_to_element_value(&Value::Date(2025, 6, 15, 13, 45, 30, 123456), Some(&c));
        assert_eq!(
            v,
            ElementValue::String(Arc::from("2025-06-15 13:45:30.123456"))
        );
    }

    #[test]
    fn test_time() {
        let v = mysql_value_to_element_value(&Value::Time(false, 1, 13, 45, 30, 500), None);
        assert_eq!(v, ElementValue::String(Arc::from("037:45:30.000500")));
    }

    #[test]
    fn test_year_bytes_parsed_as_integer() {
        let c = ctx(ColumnType::MYSQL_TYPE_YEAR, None, None, None);
        let v = mysql_value_to_element_value(&Value::Bytes(b"2025".to_vec()), Some(&c));
        assert_eq!(v, ElementValue::Integer(2025));
    }

    #[test]
    fn test_year_zero_remaps_mysql_common_offset() {
        let c = ctx(ColumnType::MYSQL_TYPE_YEAR, None, None, None);
        // mysql_common decodes YEAR wire byte 0 as Int(1900).
        let v = mysql_value_to_element_value(&Value::Int(1900), Some(&c));
        assert_eq!(v, ElementValue::Integer(0));
        let v = mysql_value_to_element_value(&Value::Bytes(b"0000".to_vec()), Some(&c));
        assert_eq!(v, ElementValue::Integer(0));
        let v = mysql_value_to_element_value(&Value::Bytes(b"1900".to_vec()), Some(&c));
        assert_eq!(v, ElementValue::Integer(0));
    }

    #[test]
    fn test_timestamp_zero_sentinel() {
        let c = ctx(ColumnType::MYSQL_TYPE_TIMESTAMP, Some(0), None, None);
        let v = mysql_value_to_element_value(&Value::Int(0), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("0000-00-00 00:00:00")));
    }

    #[test]
    fn test_enum_ordinal_to_label() {
        let c = ctx(
            ColumnType::MYSQL_TYPE_ENUM,
            None,
            Some(vec!["red".into(), "green".into(), "blue".into()]),
            None,
        );
        let v = mysql_value_to_element_value(&Value::Int(2), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("green")));
    }

    #[test]
    fn test_set_bitmask_to_labels() {
        let c = ctx(
            ColumnType::MYSQL_TYPE_SET,
            None,
            None,
            Some(vec!["a".into(), "b".into(), "c".into()]),
        );
        // 0b101 = a,c
        let v = mysql_value_to_element_value(&Value::Bytes(vec![0b101]), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("a,c")));
    }

    #[test]
    fn test_set_without_labels_falls_back_to_hex() {
        let c = ctx(ColumnType::MYSQL_TYPE_SET, None, None, None);
        let v = mysql_value_to_element_value(&Value::Bytes(vec![0b101]), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("0x05")));
    }

    #[test]
    fn test_enum_without_labels_keeps_integer() {
        let c = ctx(ColumnType::MYSQL_TYPE_ENUM, None, None, None);
        let v = mysql_value_to_element_value(&Value::Int(2), Some(&c));
        assert_eq!(v, ElementValue::Integer(2));
    }

    #[test]
    fn test_timestamp_epoch_int() {
        let c = ctx(ColumnType::MYSQL_TYPE_TIMESTAMP, Some(0), None, None);
        // 2025-06-15 13:45:30 UTC
        let secs = chrono::DateTime::parse_from_rfc3339("2025-06-15T13:45:30Z")
            .unwrap()
            .timestamp();
        let v = mysql_value_to_element_value(&Value::Int(secs), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("2025-06-15 13:45:30")));
    }

    #[test]
    fn test_timestamp2_epoch_bytes() {
        let c = ctx(ColumnType::MYSQL_TYPE_TIMESTAMP2, Some(6), None, None);
        let secs = chrono::DateTime::parse_from_rfc3339("2025-06-15T13:45:30Z")
            .unwrap()
            .timestamp();
        let payload = format!("{secs}.123456");
        let v = mysql_value_to_element_value(&Value::Bytes(payload.into_bytes()), Some(&c));
        assert_eq!(
            v,
            ElementValue::String(Arc::from("2025-06-15 13:45:30.123456"))
        );
    }

    #[test]
    fn test_json_bytes_canonicalized() {
        let c = ctx(ColumnType::MYSQL_TYPE_JSON, None, None, None);
        let v = mysql_value_to_element_value(&Value::Bytes(br#"{"k": 1}"#.to_vec()), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from(r#"{"k":1}"#)));
    }
}
