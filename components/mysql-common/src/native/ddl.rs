// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The DDL policy: which binlog statements (`QUERY_EVENT`s outside row
//! changes) the source ignores, which make it re-inspect selected tables and
//! which fence it.
//!
//! Statements that touch no selected table are ignored, including every
//! administrative statement (`ANALYZE`, `OPTIMIZE`, `GRANT`, ...). A schema
//! statement that names a selected table (`ALTER`, `RENAME`, `DROP`,
//! `CREATE INDEX`, ...) makes the worker re-read that table's catalog; only a
//! change to the facts the cursor is bound to (the selected table set, key
//! columns and their types) fences the source with
//! [`SourceFailure::SchemaChanged`](super::errors::SourceFailure). `TRUNCATE`
//! of a selected table, `DROP DATABASE` of the configured database and
//! statement-based data changes to a selected table always fence.
//!
//! The scanner is deliberately conservative: an identifier that merely
//! resembles a selected table makes the worker re-inspect, which is harmless.

/// What the source does with one statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementEffect {
    /// Nothing selected is affected.
    Ignore,
    /// Re-read the catalog of these selected tables before continuing.
    Inspect(Vec<String>),
    /// The selected history cannot be followed: a schema change.
    SchemaChanged(String),
    /// A statement-based change to selected data, which the row decoder
    /// cannot represent.
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    Dot,
    Other,
}

fn tokens(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.chars().collect();
    let mut output = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        let next = chars.get(index + 1).copied();
        match current {
            c if c.is_whitespace() => index += 1,
            '#' => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '-' if next == Some('-')
                && chars
                    .get(index + 2)
                    .is_none_or(|character| character.is_whitespace()) =>
            {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if next == Some('*') => {
                // `/*!NNNNN ... */` is executable on MySQL; scan its body.
                if chars.get(index + 2) == Some(&'!') {
                    index += 3;
                    while index < chars.len() && chars[index].is_ascii_digit() {
                        index += 1;
                    }
                } else {
                    index += 2;
                    while index + 1 < chars.len()
                        && !(chars[index] == '*' && chars[index + 1] == '/')
                    {
                        index += 1;
                    }
                    index += 2;
                }
            }
            '*' if next == Some('/') => index += 2,
            '`' | '"' => {
                let quote = current;
                let mut value = String::new();
                index += 1;
                while index < chars.len() {
                    if chars[index] == quote {
                        if chars.get(index + 1) == Some(&quote) {
                            value.push(quote);
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    value.push(chars[index]);
                    index += 1;
                }
                output.push(Token::Quoted(value));
            }
            '\'' => {
                index += 1;
                while index < chars.len() {
                    if chars[index] == '\\' {
                        index += 2;
                        continue;
                    }
                    if chars[index] == '\'' {
                        if chars.get(index + 1) == Some(&'\'') {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                output.push(Token::Other);
            }
            '.' => {
                output.push(Token::Dot);
                index += 1;
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' => {
                let mut value = String::new();
                while index < chars.len()
                    && (chars[index].is_alphanumeric() || matches!(chars[index], '_' | '$'))
                {
                    value.push(chars[index]);
                    index += 1;
                }
                output.push(Token::Word(value));
            }
            _ => {
                output.push(Token::Other);
                index += 1;
            }
        }
    }
    output
}

fn name(token: &Token) -> Option<&str> {
    match token {
        Token::Word(value) | Token::Quoted(value) => Some(value),
        _ => None,
    }
}

fn keyword(token: &Token) -> Option<String> {
    match token {
        Token::Word(value) => Some(value.to_ascii_uppercase()),
        _ => None,
    }
}

/// Selected tables referenced by `tokens`, resolving unqualified names
/// against the statement's default `schema`.
fn referenced(tokens: &[Token], schema: &str, database: &str, selected: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let Some(candidate) = name(token) else {
            continue;
        };
        if tokens.get(index + 1) == Some(&Token::Dot) {
            continue;
        }
        let qualifier = if index >= 2 && tokens[index - 1] == Token::Dot {
            name(&tokens[index - 2])
        } else {
            None
        };
        let in_database = match qualifier {
            Some(qualifier) => qualifier.eq_ignore_ascii_case(database),
            None => schema.eq_ignore_ascii_case(database),
        };
        if !in_database {
            continue;
        }
        if let Some(table) = selected
            .iter()
            .find(|table| table.eq_ignore_ascii_case(candidate))
        {
            if !found.iter().any(|existing: &String| existing == table) {
                found.push((*table).to_owned());
            }
        }
    }
    found
}

/// Administrative and object kinds whose statements never change a table's
/// columns, keys or existence.
const IGNORED_VERBS: &[&str] = &[
    "ANALYZE",
    "OPTIMIZE",
    "REPAIR",
    "CHECK",
    "CHECKSUM",
    "FLUSH",
    "GRANT",
    "REVOKE",
    "SET",
    "SAVEPOINT",
    "RELEASE",
    "XA",
    "INSTALL",
    "UNINSTALL",
    "RESET",
    "PURGE",
    "KILL",
    "DO",
    "USE",
    "LOCK",
    "UNLOCK",
    "CACHE",
    "LOAD",
];
const IGNORED_OBJECTS: &[&str] = &[
    "USER",
    "ROLE",
    "PROCEDURE",
    "FUNCTION",
    "TRIGGER",
    "EVENT",
    "VIEW",
    "SERVER",
    "LOGFILE",
    "TABLESPACE",
    "SPATIAL",
    "RESOURCE",
    "DEFINER",
    "AGGREGATE",
];

/// Classifies one statement logged in database `schema` for a source that
/// captures `selected` tables of `database`.
pub fn classify(sql: &str, schema: &str, database: &str, selected: &[&str]) -> StatementEffect {
    let tokens = tokens(sql);
    let Some(verb) = tokens.first().and_then(keyword) else {
        return StatementEffect::Ignore;
    };
    if matches!(verb.as_str(), "BEGIN" | "COMMIT" | "ROLLBACK" | "START")
        || IGNORED_VERBS.contains(&verb.as_str())
    {
        return StatementEffect::Ignore;
    }
    let touched = referenced(&tokens, schema, database, selected);
    let object = tokens
        .iter()
        .skip(1)
        .take(16)
        .filter_map(keyword)
        .find(|word| {
            matches!(
                word.as_str(),
                "TABLE" | "TABLES" | "INDEX" | "KEY" | "DATABASE" | "SCHEMA"
            ) || IGNORED_OBJECTS.contains(&word.as_str())
        });
    match verb.as_str() {
        "CREATE" | "ALTER" | "DROP" | "RENAME" => {
            if object
                .as_deref()
                .is_some_and(|object| IGNORED_OBJECTS.contains(&object))
            {
                return StatementEffect::Ignore;
            }
            if matches!(object.as_deref(), Some("DATABASE" | "SCHEMA")) {
                let drops_database = verb == "DROP"
                    && tokens.iter().skip(2).filter_map(name).any(|word| {
                        word.eq_ignore_ascii_case(database)
                            && !word.eq_ignore_ascii_case("IF")
                            && !word.eq_ignore_ascii_case("EXISTS")
                    });
                return if drops_database {
                    StatementEffect::SchemaChanged(format!("database {database} was dropped"))
                } else {
                    StatementEffect::Ignore
                };
            }
            if touched.is_empty() {
                StatementEffect::Ignore
            } else {
                StatementEffect::Inspect(touched)
            }
        }
        "TRUNCATE" => match touched.first() {
            Some(table) => StatementEffect::SchemaChanged(format!(
                "TRUNCATE of selected table {table} removed rows without row events"
            )),
            None => StatementEffect::Ignore,
        },
        _ => match touched.first() {
            Some(table) => StatementEffect::Unsupported(format!(
                "statement-based {verb} on selected table {table}; binlog_format must be ROW"
            )),
            None => StatementEffect::Ignore,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementEffect::*;

    fn effect(sql: &str) -> StatementEffect {
        classify(sql, "app", "app", &["orders", "order-items"])
    }

    #[test]
    fn statements_on_unselected_tables_and_administration_are_ignored() {
        for sql in [
            "ALTER TABLE audit_log ADD COLUMN note TEXT",
            "OPTIMIZE TABLE orders",
            "ANALYZE TABLE `orders`",
            "CREATE TABLE fresh (id INT PRIMARY KEY)",
            "DROP TABLE IF EXISTS scratch",
            "GRANT SELECT ON app.orders TO 'r'@'%'",
            "CREATE DEFINER=`root`@`%` TRIGGER t BEFORE INSERT ON orders FOR EACH ROW SET NEW.a=1",
            "CREATE OR REPLACE VIEW v AS SELECT * FROM orders",
            "DROP PROCEDURE IF EXISTS orders",
            "ALTER TABLE other.orders ADD COLUMN x INT",
            "FLUSH PRIVILEGES",
            "ALTER DATABASE app CHARACTER SET utf8mb4",
            "/* orders */ ALTER TABLE audit ADD c INT -- orders\n",
            "ALTER TABLE audit COMMENT 'orders'",
        ] {
            assert_eq!(effect(sql), Ignore, "{sql}");
        }
        assert_eq!(
            classify("ALTER TABLE orders ADD c INT", "other", "app", &["orders"]),
            Ignore,
            "unqualified names resolve against the statement's own schema"
        );
    }

    #[test]
    fn schema_statements_on_selected_tables_are_inspected() {
        for (sql, table) in [
            ("ALTER TABLE orders ADD COLUMN note TEXT", "orders"),
            ("alter table `app`.`orders` modify id bigint", "orders"),
            ("ALTER TABLE `order-items` DROP PRIMARY KEY", "order-items"),
            ("RENAME TABLE orders TO orders_old", "orders"),
            ("DROP TABLE orders", "orders"),
            ("CREATE INDEX i ON orders (a)", "orders"),
            ("/*!50001 ALTER TABLE orders ADD c INT */", "orders"),
            ("ALTER TABLE Orders ADD c INT", "orders"),
        ] {
            assert_eq!(effect(sql), Inspect(vec![table.into()]), "{sql}");
        }
        assert_eq!(
            classify("ALTER TABLE app.orders ADD c INT", "", "app", &["orders"]),
            Inspect(vec!["orders".into()]),
            "qualified names do not depend on the default schema"
        );
    }

    #[test]
    fn truncation_dropped_databases_and_statement_dml_fence() {
        assert!(matches!(effect("TRUNCATE TABLE orders"), SchemaChanged(_)));
        assert!(matches!(effect("truncate orders"), SchemaChanged(_)));
        assert_eq!(effect("TRUNCATE TABLE audit"), Ignore);
        assert!(matches!(effect("DROP DATABASE app"), SchemaChanged(_)));
        assert!(matches!(
            effect("DROP SCHEMA IF EXISTS `app`"),
            SchemaChanged(_)
        ));
        assert_eq!(effect("DROP DATABASE other"), Ignore);
        assert!(matches!(effect("UPDATE orders SET a=1"), Unsupported(_)));
        assert_eq!(effect("UPDATE audit SET a=1"), Ignore);
    }
}
