// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{connection::Connection, MySqlConfig, MySqlRetention};
use anyhow::{Context, Result};
use mysql_common::{binlog::row::BinlogRow, constants::ColumnFlags};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    pub sql_type: String,
    pub key: bool,
    pub nullable: bool,
    pub charset: Option<String>,
    pub collation: Option<u16>,
}

pub type Tables = BTreeMap<String, Vec<Column>>;

pub fn require_replication_version(connection: &Connection) -> Result<()> {
    anyhow::ensure!(
        connection.version >= (8, 0, 20),
        "native replication requires MySQL 8.0.20 or newer"
    );
    Ok(())
}

pub fn field(row: &[Option<Vec<u8>>], index: usize) -> Result<&str> {
    Ok(std::str::from_utf8(
        row.get(index)
            .and_then(Option::as_deref)
            .context("missing MySQL catalog value")?,
    )?)
}

pub async fn inspect(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<(String, Tables)> {
    inspect_mode(connection, config, true, false).await
}

pub async fn inspect_rows(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<(String, Tables)> {
    inspect_mode(
        connection,
        config,
        matches!(config.start, super::MySqlStartPosition::Snapshot { .. }),
        false,
    )
    .await
}

pub async fn inspect_positional(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<(String, Tables)> {
    inspect_mode(connection, config, false, true).await
}

async fn inspect_mode(
    connection: &mut Connection,
    config: &MySqlConfig,
    transactional: bool,
    positional: bool,
) -> Result<(String, Tables)> {
    if !positional {
        require_replication_version(connection)?;
    }
    let rows = connection
        .query(if positional {
            "SELECT @@server_uuid, @@global.binlog_format, @@global.binlog_row_image, \
             @@global.binlog_checksum"
        } else {
            "SELECT @@server_uuid, @@global.binlog_format, @@global.binlog_row_image, \
             @@global.binlog_row_metadata, @@global.binlog_checksum"
        })
        .await?;
    let [settings] = rows.values.as_slice() else {
        anyhow::bail!("invalid MySQL server settings result");
    };
    anyhow::ensure!(
        field(settings, 1)? == "ROW"
            && field(settings, 2)? == "FULL"
            && (positional || field(settings, 3)? == "FULL")
            && field(settings, if positional { 3 } else { 4 })? == "CRC32",
        "native MySQL requires ROW format, FULL row images, CRC32 checksums and the selected metadata contract"
    );
    if config.replay == Some(MySqlRetention::UntilProcessed) {
        let retention = connection
            .query(
                "SHOW GLOBAL VARIABLES WHERE Variable_name IN \
            ('binlog_expire_logs_seconds','expire_logs_days','binlog_expire_logs_auto_purge')",
            )
            .await?;
        if positional && connection.version < (8, 0, 0) {
            anyhow::ensure!(
                retention.values.iter().any(|row| {
                    field(row, 0).is_ok_and(|name| name == "expire_logs_days")
                        && field(row, 1)
                            .is_ok_and(|value| value.parse::<f64>().is_ok_and(|days| days == 0.0))
                }),
                "positional retention until processing requires MySQL 5.7 expire_logs_days=0"
            );
        } else {
            validate_retention(&retention.values)?;
        }
    }
    let server = field(settings, 0)?.to_string();
    anyhow::ensure!(!server.is_empty(), "missing MySQL server UUID");
    Ok((
        server,
        read_tables(connection, config, transactional, false, positional).await?,
    ))
}

pub async fn inspect_standalone(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<Tables> {
    read_tables(connection, config, false, true, false).await
}

async fn read_tables(
    connection: &mut Connection,
    config: &MySqlConfig,
    transactional: bool,
    standalone: bool,
    positional: bool,
) -> Result<Tables> {
    let selected = config
        .tables
        .iter()
        .map(|table| format!("'{table}'"))
        .collect::<Vec<_>>()
        .join(",");
    let selection = if config.tables.is_empty() {
        String::new()
    } else {
        format!(" AND c.TABLE_NAME IN ({selected})")
    };
    let table_kind = if standalone {
        ""
    } else {
        " AND t.TABLE_TYPE='BASE TABLE'"
    };
    let rows = connection
        .query(&format!(
            "SELECT c.TABLE_NAME, c.COLUMN_NAME, c.COLUMN_TYPE, c.COLUMN_KEY, \
        c.IS_NULLABLE, c.CHARACTER_SET_NAME, t.ENGINE, coll.ID \
        FROM information_schema.COLUMNS c JOIN information_schema.TABLES t \
        ON c.TABLE_SCHEMA=t.TABLE_SCHEMA AND c.TABLE_NAME=t.TABLE_NAME \
        LEFT JOIN information_schema.COLLATIONS coll ON c.COLLATION_NAME=coll.COLLATION_NAME \
        WHERE c.TABLE_SCHEMA='{}'{table_kind}{selection} \
        ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION",
            config.connection.database
        ))
        .await?;
    let mut tables: Tables = BTreeMap::new();
    for row in rows.values {
        let table = field(&row, 0)?;
        anyhow::ensure!(
            crate::is_valid_identifier(table) && table.len() <= 64,
            "unsupported MySQL table identifier in selected catalog"
        );
        anyhow::ensure!(
            !transactional || field(&row, 6)? == "InnoDB",
            "native MySQL coordinated snapshots and whole transactions require InnoDB"
        );
        let charset = row
            .get(5)
            .context("missing charset column")?
            .as_ref()
            .map(|value| std::str::from_utf8(value).map(str::to_string))
            .transpose()?;
        anyhow::ensure!(
            standalone
                || positional
                || charset.as_deref().is_none_or(|name| matches!(
                    name,
                    "utf8mb4" | "utf8mb3" | "utf8" | "ascii" | "binary"
                )),
            "native MySQL replication requires UTF-8, ASCII or binary columns"
        );
        let column = Column {
            name: field(&row, 1)?.to_string(),
            sql_type: field(&row, 2)?.to_string(),
            key: field(&row, 3)? == "PRI",
            nullable: field(&row, 4)? == "YES",
            charset,
            collation: row
                .get(7)
                .context("missing collation column")?
                .as_ref()
                .map(|value| -> Result<u16> { Ok(std::str::from_utf8(value)?.parse()?) })
                .transpose()?,
        };
        if positional {
            if let Some(charset) = column.charset.as_deref().filter(|name| *name != "binary") {
                crate::decoder::positional_encoding(charset)?;
            }
        }
        wire_types(&column.sql_type)?;
        let columns = tables.entry(table.to_string()).or_default();
        anyhow::ensure!(
            columns.len() < 4096,
            "MySQL column count exceeds supported limit"
        );
        columns.push(column);
    }
    anyhow::ensure!(
        config.tables.is_empty()
            || (tables.len() == config.tables.len()
                && config.tables.iter().all(|table| tables.contains_key(table))),
        "each selected MySQL table must exist"
    );
    for (table, columns) in &tables {
        let keys = if positional {
            positional_keys(config, table, columns.len())?
        } else {
            key_columns(config, table, columns)?
        };
        anyhow::ensure!(
            positional
                || keys
                    .iter()
                    .all(|key| columns.iter().any(|column| &column.name == key)),
            "native MySQL key columns must exist"
        );
    }
    anyhow::ensure!(
        config.tables.is_empty()
            || config
                .table_keys
                .iter()
                .all(|key| tables.contains_key(&key.table)),
        "configured MySQL key table is missing"
    );
    let selected = tables
        .keys()
        .map(|table| format!("'{table}'"))
        .collect::<Vec<_>>()
        .join(",");
    if selected.is_empty() {
        return Ok(tables);
    }
    let prefixes = connection
        .query(&format!(
            "SELECT TABLE_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA='{}' \
        AND TABLE_NAME IN ({selected}) AND INDEX_NAME='PRIMARY' AND SUB_PART IS NOT NULL",
            config.connection.database
        ))
        .await?;
    anyhow::ensure!(
        prefixes.values.iter().all(|row| field(row, 0)
            .is_ok_and(|table| config.table_keys.iter().any(|keys| keys.table == table))),
        "native MySQL requires full-column primary keys"
    );
    Ok(tables)
}

pub fn key_columns(config: &MySqlConfig, table: &str, columns: &[Column]) -> Result<Vec<String>> {
    if let Some(keys) = config.table_keys.iter().find(|key| key.table == table) {
        return Ok(keys.key_columns.clone());
    }
    let mut keys = columns
        .iter()
        .filter(|column| column.key)
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    if keys.is_empty() {
        if let Some(column) = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case("id"))
        {
            keys.push(column.name.clone());
        }
    }
    anyhow::ensure!(
        !keys.is_empty(),
        "MySQL table {table} needs a primary key, id column or explicit table_keys"
    );
    Ok(keys)
}

pub fn positional_keys(
    config: &MySqlConfig,
    table: &str,
    column_count: usize,
) -> Result<Vec<String>> {
    let keys = &config
        .table_keys
        .iter()
        .find(|keys| keys.table == table)
        .with_context(|| format!("positional MySQL table {table} requires explicit @N keys"))?
        .key_columns;
    anyhow::ensure!(!keys.is_empty(), "positional MySQL keys are empty");
    for key in keys {
        let index: usize = key
            .strip_prefix('@')
            .context("positional MySQL keys must use @N column offsets")?
            .parse()?;
        anyhow::ensure!(
            index < column_count && key == &format!("@{index}"),
            "invalid positional MySQL key offset"
        );
    }
    Ok(keys.clone())
}

pub fn validate_positional_row(columns: &[Column], row: &BinlogRow) -> Result<()> {
    anyhow::ensure!(
        row.len() == columns.len() && row.columns_ref().len() == columns.len(),
        "MySQL positional column count changed"
    );
    for (index, (expected, actual)) in columns.iter().zip(row.columns_ref()).enumerate() {
        anyhow::ensure!(
            wire_types(&expected.sql_type)?.contains(&actual.column_type()),
            "MySQL positional column {index} type changed; explicit schema recovery is required"
        );
        let value = row
            .as_ref(index)
            .context("incomplete MySQL positional row")?;
        anyhow::ensure!(
            expected.nullable
                || !matches!(
                    value,
                    mysql_common::binlog::value::BinlogValue::Value(mysql_common::Value::NULL)
                ),
            "NULL in a non-null MySQL positional column"
        );
    }
    Ok(())
}

fn validate_retention(rows: &[Vec<Option<Vec<u8>>>]) -> Result<()> {
    let mut settings = BTreeMap::new();
    for row in rows {
        anyhow::ensure!(
            settings.insert(field(row, 0)?, field(row, 1)?).is_none(),
            "duplicate MySQL retention setting"
        );
    }
    let seconds: u64 = settings
        .get("binlog_expire_logs_seconds")
        .context("missing MySQL binlog retention setting")?
        .parse()?;
    let days = settings
        .get("expire_logs_days")
        .map(|value| value.parse::<u64>())
        .transpose()?;
    let disabled = match settings.get("binlog_expire_logs_auto_purge").copied() {
        Some("OFF") => true,
        Some("ON") | None => seconds == 0 && days.is_none_or(|days| days == 0),
        Some(_) => anyhow::bail!("invalid MySQL automatic binlog purge setting"),
    };
    anyhow::ensure!(
        disabled,
        "retention until processing requires automatic MySQL binlog expiry disabled"
    );
    Ok(())
}

pub fn validate_row(columns: &[Column], row: &BinlogRow) -> Result<()> {
    anyhow::ensure!(
        row.len() == columns.len() && row.columns_ref().len() == columns.len(),
        "MySQL column count changed"
    );
    for (index, (expected, actual)) in columns.iter().zip(row.columns_ref()).enumerate() {
        use mysql_common::{binlog::value::BinlogValue, Value};
        let sql_type = expected
            .sql_type
            .split(['(', ' '])
            .next()
            .context("missing SQL type")?;
        let compatible = wire_types(&expected.sql_type)?.contains(&actual.column_type());
        let unsigned = sql_type == "year"
            || (!matches!(sql_type, "enum" | "set")
                && expected
                    .sql_type
                    .split_whitespace()
                    .any(|word| word == "unsigned"));
        let binary = matches!(
            sql_type,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob"
        );
        let collation = expected.collation.or(binary.then_some(63));
        anyhow::ensure!(
            compatible && expected.name.as_bytes() == actual.name_ref()
                && collation.is_none_or(|collation| collation == actual.character_set())
                && expected.key == actual.flags().contains(ColumnFlags::PRI_KEY_FLAG)
                && unsigned == actual.flags().contains(ColumnFlags::UNSIGNED_FLAG),
            "MySQL column {} identity, type, charset, key or signedness changed: expected {sql_type}, charset {collation:?}, key {}, unsigned {unsigned}; got {} {:?}, charset {}, key {}, unsigned {}; explicit schema recovery is required",
            expected.name, expected.key, actual.name_str(), actual.column_type(),
            actual.character_set(), actual.flags().contains(ColumnFlags::PRI_KEY_FLAG),
            actual.flags().contains(ColumnFlags::UNSIGNED_FLAG)
        );
        let value = row.as_ref(index).context("incomplete MySQL row")?;
        anyhow::ensure!(
            expected.nullable || !matches!(value, BinlogValue::Value(Value::NULL)),
            "NULL in non-null MySQL column"
        );
    }
    Ok(())
}

pub fn validate_snapshot_columns(
    expected: &[Column],
    actual: &[mysql_common::packets::Column],
) -> Result<()> {
    use mysql_common::constants::ColumnType::*;
    anyhow::ensure!(
        expected.len() == actual.len(),
        "MySQL snapshot column count changed"
    );
    for (expected, actual) in expected.iter().zip(actual) {
        let sql_type = expected
            .sql_type
            .split(['(', ' '])
            .next()
            .context("missing SQL type")?;
        let compatible = match sql_type {
            "enum" => {
                actual.column_type() == MYSQL_TYPE_STRING
                    && actual.flags().contains(ColumnFlags::ENUM_FLAG)
            }
            "set" => {
                actual.column_type() == MYSQL_TYPE_STRING
                    && actual.flags().contains(ColumnFlags::SET_FLAG)
            }
            "tinyblob" | "tinytext" => {
                matches!(actual.column_type(), MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_BLOB)
            }
            "mediumblob" | "mediumtext" => matches!(
                actual.column_type(),
                MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_BLOB
            ),
            "longblob" | "longtext" => {
                matches!(actual.column_type(), MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB)
            }
            _ => wire_types(&expected.sql_type)?.contains(&actual.column_type()),
        };
        let unsigned = matches!(sql_type, "year" | "bit")
            || (!matches!(sql_type, "enum" | "set")
                && expected
                    .sql_type
                    .split_whitespace()
                    .any(|word| word == "unsigned"));
        anyhow::ensure!(
            compatible
                && expected.name.as_bytes() == actual.name_ref()
                && unsigned == actual.flags().contains(ColumnFlags::UNSIGNED_FLAG),
            "MySQL snapshot column {} identity, type or signedness changed: expected {sql_type}, unsigned {unsigned}; got {} {:?}, unsigned {}",
            expected.name, actual.name_str(), actual.column_type(),
            actual.flags().contains(ColumnFlags::UNSIGNED_FLAG)
        );
    }
    Ok(())
}

fn wire_types(sql_type: &str) -> Result<&'static [mysql_common::constants::ColumnType]> {
    use mysql_common::constants::ColumnType::*;
    Ok(
        match sql_type
            .split(['(', ' '])
            .next()
            .context("missing SQL type")?
        {
            "tinyint" => &[MYSQL_TYPE_TINY],
            "smallint" => &[MYSQL_TYPE_SHORT],
            "mediumint" => &[MYSQL_TYPE_INT24],
            "int" => &[MYSQL_TYPE_LONG],
            "bigint" => &[MYSQL_TYPE_LONGLONG],
            "float" => &[MYSQL_TYPE_FLOAT],
            "double" => &[MYSQL_TYPE_DOUBLE],
            "decimal" => &[MYSQL_TYPE_NEWDECIMAL],
            "year" => &[MYSQL_TYPE_YEAR],
            "date" => &[MYSQL_TYPE_DATE, MYSQL_TYPE_NEWDATE],
            "time" => &[MYSQL_TYPE_TIME, MYSQL_TYPE_TIME2],
            "datetime" => &[MYSQL_TYPE_DATETIME, MYSQL_TYPE_DATETIME2],
            "timestamp" => &[MYSQL_TYPE_TIMESTAMP, MYSQL_TYPE_TIMESTAMP2],
            "varchar" | "varbinary" => &[MYSQL_TYPE_VARCHAR, MYSQL_TYPE_VAR_STRING],
            "char" | "binary" => &[MYSQL_TYPE_STRING],
            "tinytext" | "text" | "mediumtext" | "longtext" | "tinyblob" | "blob"
            | "mediumblob" | "longblob" => &[MYSQL_TYPE_BLOB],
            "bit" => &[MYSQL_TYPE_BIT],
            "enum" => &[MYSQL_TYPE_ENUM],
            "set" => &[MYSQL_TYPE_SET],
            "json" => &[MYSQL_TYPE_JSON],
            _ => anyhow::bail!("unsupported native MySQL column type {sql_type}"),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_disables_both_legacy_and_current_expiry_settings() {
        let validate = |values: &[(&str, &str)]| {
            let rows = values
                .iter()
                .map(|(name, value)| {
                    vec![
                        Some(name.as_bytes().to_vec()),
                        Some(value.as_bytes().to_vec()),
                    ]
                })
                .collect::<Vec<_>>();
            validate_retention(&rows)
        };
        assert!(validate(&[]).is_err());
        assert!(validate(&[("binlog_expire_logs_seconds", "0")]).is_ok());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("expire_logs_days", "1")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "3600"),
            ("expire_logs_days", "0")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("expire_logs_days", "0"),
            ("binlog_expire_logs_auto_purge", "ON")
        ])
        .is_ok());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "3600"),
            ("expire_logs_days", "0"),
            ("binlog_expire_logs_auto_purge", "OFF")
        ])
        .is_ok());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("binlog_expire_logs_auto_purge", "unknown")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("expire_logs_days", "bad")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("binlog_expire_logs_seconds", "0")
        ])
        .is_err());
    }
}
