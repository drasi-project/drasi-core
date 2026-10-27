// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{catalog::SqlClient, Catalog, Cursor, NativeConfig, Table};
use crate::{decoder::CdcOperation, extract_column_value, value_to_string, Lsn};
use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{ChangeOperation, GraphChangeCodec, StreamId};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use std::sync::Arc;

pub(super) type CdcRow = ([u8; 10], [u8; 10], i32, i32, Element);
pub struct Change {
    pub change: SourceChange,
    pub before: Option<Element>,
    pub position: Cursor,
}
impl Change {
    /// Exact bytes of the actual serialized graph record payloads. UPDATE counts
    /// both full images; DELETE counts its full before image. Transport framing,
    /// cursor JSON and Rust allocation overhead are deliberately not this metric.
    pub fn size(&self) -> Result<usize> {
        let frame = GraphChangeCodec::encode_change(
            self.change.clone(),
            StreamId::try_new("mssql/byte-accounting")?,
            1,
            None,
        )?;
        let [operation] = frame.changes().operations() else {
            anyhow::bail!("SQL Server change must contain one operation")
        };
        let (after, prior) = match operation {
            ChangeOperation::Added { after, .. } => (after.payload().len(), None),
            ChangeOperation::Updated { after, before, .. } => {
                (after.payload().len(), before.as_ref())
            }
            ChangeOperation::Deleted { before, .. } => (0, before.as_ref()),
        };
        let before = self
            .before
            .as_ref()
            .map(element_size)
            .transpose()?
            .unwrap_or_else(|| prior.map_or(0, |record| record.payload().len()));
        after
            .checked_add(before)
            .context("SQL Server record byte count overflow")
    }
}
pub struct Poll {
    pub catalog: Catalog,
    pub changes: Vec<Change>,
    pub boundary: Cursor,
}
pub async fn poll(config: &NativeConfig, source: &str, resume: Option<&Cursor>) -> Result<Poll> {
    super::paging::poll(config, source, resume).await
}
pub(super) async fn lsn(
    client: &mut SqlClient,
    query: &str,
    params: &[&dyn tiberius::ToSql],
) -> Result<[u8; 10]> {
    let rows = client
        .query(query, params)
        .await?
        .into_first_result()
        .await?;
    let bytes: &[u8] = rows
        .first()
        .context("missing CDC LSN row")?
        .try_get(0)?
        .context("missing CDC LSN")?;
    Ok(*Lsn::from_bytes(bytes)?.as_bytes())
}
pub fn element_size(element: &Element) -> Result<usize> {
    let frame = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: element.clone(),
        },
        StreamId::try_new("mssql/byte-accounting")?,
        1,
        None,
    )?;
    let [ChangeOperation::Added { after, .. }] = frame.changes().operations() else {
        anyhow::bail!("SQL Server row did not produce a graph record");
    };
    Ok(after.payload().len())
}
pub(super) fn decode_rows(mut raw: Vec<CdcRow>, binding: [u8; 32]) -> Result<Vec<Change>> {
    raw.sort_by_key(|(lsn, seq, table, op, _)| (*lsn, *seq, *table, *op));
    let mut changes = Vec::new();
    let mut iter = raw.into_iter();
    while let Some((lsn, seq, table, op, element)) = iter.next() {
        let position = |ordinal| Cursor {
            version: 1,
            binding,
            lsn,
            row: Some((seq, table, ordinal)),
        };
        match CdcOperation::from_i32(op)? {
            CdcOperation::Insert => changes.push(Change {
                change: SourceChange::Insert { element },
                before: None,
                position: position(0),
            }),
            CdcOperation::Delete => changes.push(Change {
                change: SourceChange::Delete {
                    metadata: element.get_metadata().clone(),
                },
                before: Some(element),
                position: position(0),
            }),
            CdcOperation::UpdateBefore => {
                let (next_lsn, next_seq, next_table, next_op, after) =
                    iter.next().context("CDC update after-image is missing")?;
                ensure!(
                    (lsn, seq, table, 4) == (next_lsn, next_seq, next_table, next_op),
                    "CDC update before/after images do not pair"
                );
                if element.get_metadata().reference == after.get_metadata().reference {
                    changes.push(Change {
                        change: SourceChange::Replace { element: after },
                        before: Some(element),
                        position: position(0),
                    });
                } else {
                    changes.push(Change {
                        change: SourceChange::Delete {
                            metadata: element.get_metadata().clone(),
                        },
                        before: Some(element),
                        position: position(0),
                    });
                    changes.push(Change {
                        change: SourceChange::Insert { element: after },
                        before: None,
                        position: position(1),
                    });
                }
            }
            CdcOperation::UpdateAfter => anyhow::bail!("unpaired SQL Server CDC after-image"),
        }
    }
    Ok(changes)
}
pub(super) fn element(
    source: &str,
    table: &Table,
    row: &tiberius::Row,
    timestamp: u64,
) -> Result<Element> {
    let mut properties = ElementPropertyMap::new();
    for (name, ..) in &table.columns {
        let index = row
            .columns()
            .iter()
            .position(|c| c.name() == name)
            .context("captured SQL Server column is missing")?;
        properties.insert(name.as_str(), extract_column_value(row, index)?);
    }
    let mut key = Vec::new();
    for name in &table.keys {
        let value = properties
            .get(name.as_str())
            .context("SQL Server key column missing")?;
        ensure!(
            !matches!(value, ElementValue::Null),
            "SQL Server primary/custom keys must not be NULL"
        );
        let value = value_to_string(value);
        key.push(if table.keys.len() == 1 {
            value
        } else {
            value.replace('%', "%25").replace('_', "%5F")
        });
    }
    Ok(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(
                source,
                &format!("{}:{}", table.canonical(), key.join("_")),
            ),
            labels: Arc::from([Arc::from(table.name.as_str())]),
            effective_from: timestamp,
        },
        properties,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;
    fn node(id: &str, value: i64) -> Element {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::Integer(value));
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::from([Arc::from("Items")]),
                effective_from: 123,
            },
            properties,
        }
    }
    fn row(lsn: u8, seq: u8, table: i32, operation: i32, id: &str, value: i64) -> CdcRow {
        ([lsn; 10], [seq; 10], table, operation, node(id, value))
    }
    #[test]
    fn insert_delete_and_update_preserve_full_images() -> Result<()> {
        let changes = decode_rows(
            vec![
                row(1, 1, 2, 2, "dbo.Items:1", 10),
                row(2, 1, 2, 3, "dbo.Items:1", 10),
                row(2, 1, 2, 4, "dbo.Items:1", 20),
                row(3, 1, 2, 1, "dbo.Items:1", 20),
            ],
            [7; 32],
        )?;
        assert_eq!(changes.len(), 3);
        assert!(matches!(changes[0].change, SourceChange::Insert { .. }));
        assert_eq!(changes[1].before.as_ref(), Some(&node("dbo.Items:1", 10)));
        assert!(matches!(changes[1].change, SourceChange::Replace { .. }));
        assert_eq!(changes[2].before.as_ref(), Some(&node("dbo.Items:1", 20)));
        assert!(matches!(changes[2].change, SourceChange::Delete { .. }));
        Ok(())
    }
    #[test]
    fn rekey_has_two_replayable_ordinals_without_losing_old_key() -> Result<()> {
        let changes = decode_rows(
            vec![
                row(1, 1, 2, 3, "dbo.Items:1", 10),
                row(1, 1, 2, 4, "dbo.Items:2", 20),
            ],
            [7; 32],
        )?;
        assert_eq!(changes.len(), 2);
        assert!(
            matches!(&changes[0].change,SourceChange::Delete {metadata} if metadata.reference.element_id.as_ref()=="dbo.Items:1")
        );
        assert!(
            matches!(&changes[1].change,SourceChange::Insert {element} if element.get_metadata().reference.element_id.as_ref()=="dbo.Items:2")
        );
        assert_eq!(
            changes[0].position.compare(&changes[1].position)?,
            Ordering::Less
        );
        Ok(())
    }
    #[test]
    fn changes_are_globally_ordered_not_grouped_by_selected_table() -> Result<()> {
        let changes = decode_rows(
            vec![
                row(2, 2, 3, 2, "sales.Items:2", 20),
                row(1, 1, 2, 2, "dbo.Items:1", 10),
                row(2, 1, 3, 2, "sales.Items:1", 15),
            ],
            [7; 32],
        )?;
        assert_eq!(changes[0].position.lsn, [1; 10]);
        assert_eq!(changes[1].position.row, Some(([1; 10], 3, 0)));
        assert_eq!(changes[2].position.row, Some(([2; 10], 3, 0)));
        Ok(())
    }
    #[test]
    fn missing_mismatched_and_invalid_images_fail_without_partial_output() {
        for rows in [
            vec![row(1, 1, 2, 4, "a", 1)],
            vec![row(1, 1, 2, 3, "a", 1)],
            vec![row(1, 1, 2, 3, "a", 1), row(2, 1, 2, 4, "a", 2)],
            vec![row(1, 1, 2, 2, "a", 1), row(2, 1, 2, 8, "a", 2)],
        ] {
            assert!(decode_rows(rows, [7; 32]).is_err());
        }
    }
    #[test]
    fn byte_metric_is_the_real_record_codec_and_counts_both_update_images() -> Result<()> {
        let before = node("dbo.Items:1", 10);
        let after = node("dbo.Items:1", 20);
        let changes = decode_rows(
            vec![
                row(1, 1, 2, 3, "dbo.Items:1", 10),
                row(1, 1, 2, 4, "dbo.Items:1", 20),
            ],
            [7; 32],
        )?;
        assert_eq!(
            changes[0].size()?,
            element_size(&before)? + element_size(&after)?
        );
        let frame = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: after.clone(),
            },
            StreamId::try_new("different/transport")?,
            999,
            None,
        )?;
        let ChangeOperation::Added { after: record, .. } = &frame.changes().operations()[0] else {
            panic!("insert")
        };
        assert_eq!(element_size(&after)?, record.payload().len());
        Ok(())
    }
}
