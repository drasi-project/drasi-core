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

//! Oracle LogMiner helpers.

use crate::scn::Scn;
use anyhow::{anyhow, Result};
use oracle::Connection;
use std::collections::HashMap;

pub struct LogMinerGuard<'a> {
    conn: &'a Connection,
    active: bool,
}

impl<'a> LogMinerGuard<'a> {
    pub fn new(conn: &'a Connection, start_scn: Scn, end_scn: Scn) -> Result<Self> {
        let sql = format!(
            "BEGIN DBMS_LOGMNR.START_LOGMNR(\n                STARTSCN => {},\n                ENDSCN   => {},\n                OPTIONS  => DBMS_LOGMNR.DICT_FROM_ONLINE_CATALOG\n                          + DBMS_LOGMNR.COMMITTED_DATA_ONLY\n                          + DBMS_LOGMNR.NO_SQL_DELIMITER\n            ); END;",
            start_scn.0, end_scn.0
        );
        conn.execute(&sql, &[])?;
        Ok(Self { conn, active: true })
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.active {
            self.conn
                .execute("BEGIN DBMS_LOGMNR.END_LOGMNR; END;", &[])?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for LogMinerGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.conn.execute("BEGIN DBMS_LOGMNR.END_LOGMNR; END;", &[]);
            self.active = false;
        }
    }
}

pub fn parse_sql_undo_insert(sql_undo: &str) -> Result<HashMap<String, String>> {
    let col_start = sql_undo
        .find('(')
        .ok_or_else(|| anyhow!("No '(' found in SQL_UNDO: {sql_undo}"))?;
    let col_end = sql_undo[col_start..]
        .find(')')
        .ok_or_else(|| anyhow!("No ')' found in SQL_UNDO column list: {sql_undo}"))?
        + col_start;
    let columns = &sql_undo[col_start + 1..col_end];

    let values_index = sql_undo
        .to_uppercase()
        .find("VALUES")
        .ok_or_else(|| anyhow!("No VALUES clause found in SQL_UNDO: {sql_undo}"))?;
    let value_start = sql_undo[values_index..]
        .find('(')
        .ok_or_else(|| anyhow!("No '(' found after VALUES in SQL_UNDO: {sql_undo}"))?
        + values_index;
    let value_end = sql_undo
        .rfind(')')
        .ok_or_else(|| anyhow!("No closing ')' found in SQL_UNDO: {sql_undo}"))?;
    let values = &sql_undo[value_start + 1..value_end];

    let column_names = columns
        .split(',')
        .map(|column| column.trim().trim_matches('"').to_lowercase())
        .collect::<Vec<_>>();
    let parsed_values = split_sql_values(values);

    if column_names.len() != parsed_values.len() {
        return Err(anyhow!(
            "SQL_UNDO column count {} does not match value count {}: {sql_undo}",
            column_names.len(),
            parsed_values.len()
        ));
    }

    Ok(column_names.into_iter().zip(parsed_values).collect())
}

pub fn parse_sql_undo_update(sql_undo: &str) -> Result<HashMap<String, String>> {
    let upper = sql_undo.to_uppercase();
    let set_index = upper
        .find(" SET ")
        .ok_or_else(|| anyhow!("No SET clause found in SQL_UNDO: {sql_undo}"))?;
    let where_index = upper[set_index + 5..]
        .find(" WHERE ")
        .map(|index| index + set_index + 5)
        .unwrap_or(sql_undo.len());
    let assignments = &sql_undo[set_index + 5..where_index];

    let mut parsed = HashMap::new();
    for assignment in split_sql_values(assignments) {
        let (column, value) = assignment
            .split_once('=')
            .ok_or_else(|| anyhow!("Invalid assignment in SQL_UNDO: {assignment}"))?;
        parsed.insert(
            column.trim().trim_matches('"').to_lowercase(),
            value.trim().to_string(),
        );
    }

    Ok(parsed)
}

pub fn split_sql_values(values: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut paren_depth = 0i32;
    let mut chars = values.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_string => {
                in_string = true;
                current.push(ch);
            }
            '\'' if in_string => {
                current.push(ch);
                if chars.peek() == Some(&'\'') {
                    current.push(chars.next().expect("peeked escaped quote must exist"));
                } else {
                    in_string = false;
                }
            }
            '(' if !in_string => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' if !in_string => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                }
                current.push(ch);
            }
            ',' if !in_string && paren_depth == 0 => {
                result.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        result.push(current.trim().to_string());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_sql_values() {
        let values = split_sql_values("1, 'O''Brien', TO_DATE('2024-01-01', 'YYYY-MM-DD'), NULL");
        assert_eq!(values.len(), 4);
        assert_eq!(values[1], "'O''Brien'");
        assert_eq!(values[2], "TO_DATE('2024-01-01', 'YYYY-MM-DD')");
    }

    #[test]
    fn test_parse_sql_undo_insert() {
        let parsed = parse_sql_undo_insert(
            r#"INSERT INTO "SYSTEM"."DRASI_TEST"("ID","NAME") VALUES (2,'Bob')"#,
        )
        .unwrap();
        assert_eq!(parsed.get("id").map(String::as_str), Some("2"));
        assert_eq!(parsed.get("name").map(String::as_str), Some("'Bob'"));
    }

    #[test]
    fn test_parse_sql_undo_update() {
        let parsed = parse_sql_undo_update(
            r#"update "SYSTEM"."DRASI_TEST" set "NAME" = 'Bob', "UPDATED_AT" = TO_DATE('2024-01-01', 'YYYY-MM-DD') where "ID" = 2"#,
        )
        .unwrap();
        assert_eq!(parsed.get("name").map(String::as_str), Some("'Bob'"));
        assert_eq!(
            parsed.get("updated_at").map(String::as_str),
            Some("TO_DATE('2024-01-01', 'YYYY-MM-DD')")
        );
    }
}
