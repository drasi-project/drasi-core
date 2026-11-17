// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! SQL Server object names in configuration.
//!
//! A name part is either a regular identifier (letters, digits, `_`, `@`,
//! `#`, `$`; not starting with a digit or `$`) or a bracket-quoted
//! identifier (`[Order Lines]`, with `]]` for a literal `]`). Every name is
//! re-quoted with [`quoted`] before it reaches SQL text, and catalog lookups
//! bind names as parameters, so quoting is the only escaping rule.

use anyhow::{bail, ensure, Result};

/// Longest SQL Server identifier, in characters.
pub const MAX_IDENTIFIER_CHARS: usize = 128;

/// Bracket-quotes one identifier part.
pub fn quoted(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Splits `name` into unquoted parts at unquoted dots.
fn parts(name: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut chars = name.chars().peekable();
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'[') {
            chars.next();
            loop {
                match chars.next() {
                    Some(']') if chars.peek() == Some(&']') => {
                        chars.next();
                        part.push(']');
                    }
                    Some(']') => break,
                    Some(c) => part.push(c),
                    None => bail!("unterminated [quoted] identifier in {name:?}"),
                }
            }
            ensure!(!part.is_empty(), "empty [quoted] identifier in {name:?}");
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' {
                    break;
                }
                part.push(c);
                chars.next();
            }
            let mut characters = part.chars();
            ensure!(
                characters
                    .next()
                    .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '@' || first == '#')
                    && characters.all(|c| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$')),
                "invalid SQL Server identifier {part:?} in {name:?}; bracket-quote names with other characters, as in [Order Lines]"
            );
        }
        ensure!(
            part.chars().count() <= MAX_IDENTIFIER_CHARS && !part.contains('\0'),
            "SQL Server identifier in {name:?} exceeds {MAX_IDENTIFIER_CHARS} characters or contains NUL"
        );
        parts.push(part);
        match chars.next() {
            None => return Ok(parts),
            Some('.') => continue,
            Some(c) => bail!("unexpected {c:?} after a quoted identifier in {name:?}"),
        }
    }
}

/// A schema-qualified table name. A bare name is in `dbo`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableName {
    pub schema: String,
    pub table: String,
}

impl TableName {
    pub fn parse(name: &str) -> Result<Self> {
        let parts = parts(name)?;
        match parts.as_slice() {
            [table] => Ok(Self {
                schema: "dbo".into(),
                table: table.clone(),
            }),
            [schema, table] => Ok(Self {
                schema: schema.clone(),
                table: table.clone(),
            }),
            _ => bail!("SQL Server table {name:?} must be table or schema.table"),
        }
    }

    /// Case-insensitive identity, matching SQL Server's default collation.
    pub fn key(&self) -> (String, String) {
        (self.schema.to_lowercase(), self.table.to_lowercase())
    }

    /// Whether this names `schema.table` (case-insensitively).
    pub fn matches(&self, schema: &str, table: &str) -> bool {
        self.schema.to_lowercase() == schema.to_lowercase()
            && self.table.to_lowercase() == table.to_lowercase()
    }

    /// `[schema].[table]`.
    pub fn sql(&self) -> String {
        format!("{}.{}", quoted(&self.schema), quoted(&self.table))
    }
}

/// One column name (no dots outside brackets).
pub fn column_name(name: &str) -> Result<String> {
    let parts = parts(name)?;
    let [column] = parts.as_slice() else {
        bail!("column {name:?} must be a single identifier");
    };
    Ok(column.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_and_quoted_names_parse_and_requote() {
        assert_eq!(
            TableName::parse("Orders").unwrap(),
            TableName {
                schema: "dbo".into(),
                table: "Orders".into()
            }
        );
        let quoted_name = TableName::parse("[Sales Ops].[Order]]Lines]").unwrap();
        assert_eq!(quoted_name.schema, "Sales Ops");
        assert_eq!(quoted_name.table, "Order]Lines");
        assert_eq!(quoted_name.sql(), "[Sales Ops].[Order]]Lines]");
        assert_eq!(
            TableName::parse("[a.b].c").unwrap().schema,
            "a.b",
            "dots inside brackets are part of the name"
        );
        assert!(TableName::parse("sales.Orders")
            .unwrap()
            .matches("SALES", "orders"));
        assert_eq!(column_name("[order id]").unwrap(), "order id");
        assert_eq!(column_name("Ünïcode_1").unwrap(), "Ünïcode_1");
    }

    #[test]
    fn injection_and_ambiguous_names_are_rejected() {
        for name in [
            "",
            "db.dbo.orders",
            "dbo..orders",
            "1table",
            "orders;drop",
            "orders OR 1=1",
            "[unterminated",
            "[]",
            "[a]b",
            "orders\0",
        ] {
            assert!(TableName::parse(name).is_err(), "{name:?}");
        }
        assert!(column_name("a.b").is_err());
        assert!(TableName::parse(&"a".repeat(129)).is_err());
    }
}
