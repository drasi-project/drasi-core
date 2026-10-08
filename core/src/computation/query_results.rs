// Copyright 2026 The Drasi Authors.
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

use std::collections::{btree_map::Entry, BTreeMap};

use crate::evaluation::context::{
    query_variables_unchanged, QueryPartEvaluationContext as Context, QueryVariables,
};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransactionResultError {
    #[error("transaction results have a discontinuous row at signature {signature}")]
    DiscontinuousRow { signature: u64 },
    #[error("transaction results change row kind at signature {signature}")]
    ChangedRowKind { signature: u64 },
    #[error("transaction results change grouping keys at signature {signature}")]
    ChangedGrouping { signature: u64 },
}

enum Delta {
    Row {
        before: Option<QueryVariables>,
        after: Option<QueryVariables>,
    },
    Aggregation {
        before: Option<QueryVariables>,
        after: QueryVariables,
        grouping_keys: Vec<String>,
        default_before: bool,
        default_after: bool,
    },
}

impl Delta {
    fn merge(&mut self, next: Self, signature: u64) -> Result<(), TransactionResultError> {
        match (self, next) {
            (
                Self::Row { after, .. },
                Self::Row {
                    before: next_before,
                    after: next_after,
                },
            ) => {
                let continuous = match (after.as_ref(), next_before.as_ref()) {
                    (None, None) => true,
                    (Some(current), Some(next)) => {
                        current.len() == next.len()
                            && current
                                .iter()
                                .zip(next)
                                .all(|((key, value), (next_key, next))| {
                                    key == next_key && value.eq_for_groupby(next)
                                })
                    }
                    _ => false,
                };
                if !continuous {
                    return Err(TransactionResultError::DiscontinuousRow { signature });
                }
                *after = next_after;
            }
            (
                Self::Aggregation {
                    after,
                    grouping_keys,
                    default_after,
                    ..
                },
                Self::Aggregation {
                    after: next_after,
                    grouping_keys: next_keys,
                    default_after: next_default,
                    ..
                },
            ) => {
                if *grouping_keys != next_keys {
                    return Err(TransactionResultError::ChangedGrouping { signature });
                }
                // Aggregation before/default values can be snapshots rather than
                // the preceding contribution. Keep the first baseline unchanged.
                *after = next_after;
                *default_after = next_default;
            }
            _ => return Err(TransactionResultError::ChangedRowKind { signature }),
        }
        Ok(())
    }

    fn into_context(self, row_signature: u64) -> Option<Context> {
        match self {
            Self::Row {
                before: None,
                after: None,
            } => None,
            Self::Row {
                before: None,
                after: Some(after),
            } => Some(Context::Adding {
                after,
                row_signature,
            }),
            Self::Row {
                before: Some(before),
                after: None,
            } => Some(Context::Removing {
                before,
                row_signature,
            }),
            Self::Row {
                before: Some(before),
                after: Some(after),
            } => (!query_variables_unchanged(&before, &after)).then_some(Context::Updating {
                before,
                after,
                row_signature,
            }),
            Self::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
            } => {
                if !default_before
                    && before
                        .as_ref()
                        .is_some_and(|before| query_variables_unchanged(before, &after))
                {
                    return None;
                }
                Some(Context::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    default_before,
                    default_after,
                    row_signature,
                })
            }
        }
    }
}

pub(super) fn transaction_results(
    results: Vec<Context>,
) -> Result<Vec<Context>, TransactionResultError> {
    let mut rows = BTreeMap::<u64, Delta>::new();
    for result in results {
        let signature = result.row_signature();
        let next = match result {
            Context::Adding { after, .. } => Delta::Row {
                before: None,
                after: Some(after),
            },
            Context::Updating { before, after, .. } => Delta::Row {
                before: Some(before),
                after: Some(after),
            },
            Context::Removing { before, .. } => Delta::Row {
                before: Some(before),
                after: None,
            },
            Context::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
                ..
            } => Delta::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
            },
            Context::Noop => continue,
        };
        match rows.entry(signature) {
            Entry::Vacant(entry) => {
                entry.insert(next);
            }
            Entry::Occupied(mut entry) => entry.get_mut().merge(next, signature)?,
        }
    }
    Ok(rows
        .into_iter()
        .filter_map(|(signature, delta)| delta.into_context(signature))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::variable_value::VariableValue;

    fn row(value: i64) -> QueryVariables {
        QueryVariables::from([("value".into(), VariableValue::Integer(value.into()))])
    }

    fn add(signature: u64, value: i64) -> Context {
        Context::Adding {
            after: row(value),
            row_signature: signature,
        }
    }

    fn update(signature: u64, before: i64, after: i64) -> Context {
        Context::Updating {
            before: row(before),
            after: row(after),
            row_signature: signature,
        }
    }

    fn remove(signature: u64, value: i64) -> Context {
        Context::Removing {
            before: row(value),
            row_signature: signature,
        }
    }

    fn aggregate(
        before: Option<i64>,
        after: i64,
        default_before: bool,
        default_after: bool,
    ) -> Context {
        Context::Aggregation {
            before: before.map(row),
            after: row(after),
            grouping_keys: Vec::new(),
            default_before,
            default_after,
            row_signature: 1,
        }
    }

    #[test]
    fn repeated_rows_keep_only_original_before_and_final_after() {
        let cases = [
            (
                vec![add(1, 1), update(1, 1, 2), update(1, 2, 3)],
                vec![add(1, 3)],
            ),
            (vec![add(1, 1), remove(1, 1)], vec![]),
            (
                vec![update(1, 1, 2), update(1, 2, 3)],
                vec![update(1, 1, 3)],
            ),
            (vec![update(1, 1, 2), update(1, 2, 1)], vec![]),
            (vec![update(1, 1, 2), remove(1, 2)], vec![remove(1, 1)]),
            (vec![remove(1, 1), add(1, 2)], vec![update(1, 1, 2)]),
            (vec![remove(1, 1), add(1, 1)], vec![]),
            (vec![add(1, 1), remove(1, 1), add(1, 2)], vec![add(1, 2)]),
            (
                vec![remove(1, 1), add(1, 2), remove(1, 2)],
                vec![remove(1, 1)],
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(transaction_results(input).unwrap(), expected);
        }
    }

    #[test]
    fn identities_remain_distinct_and_output_order_is_deterministic() {
        assert_eq!(
            transaction_results(vec![
                add(9, 1),
                Context::Noop,
                add(0, 1),
                update(9, 1, 3),
                remove(8, 2),
            ])
            .unwrap(),
            vec![add(0, 1), remove(8, 2), add(9, 3)]
        );
        assert!(transaction_results(vec![Context::Noop]).unwrap().is_empty());
        assert!(transaction_results(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn aggregation_preserves_first_baseline_and_last_default() {
        assert_eq!(
            transaction_results(vec![
                aggregate(Some(0), 1, true, false),
                aggregate(Some(1), 0, false, true),
                aggregate(Some(0), 2, true, false),
            ])
            .unwrap(),
            vec![aggregate(Some(0), 2, true, false)]
        );
        assert_eq!(
            transaction_results(vec![
                aggregate(None, 1, true, false),
                aggregate(Some(1), 0, false, true),
            ])
            .unwrap(),
            vec![aggregate(None, 0, true, true)]
        );
        assert!(transaction_results(vec![
            aggregate(Some(1), 2, false, false),
            aggregate(Some(2), 1, false, false),
        ])
        .unwrap()
        .is_empty());
        assert_eq!(
            transaction_results(vec![aggregate(Some(0), 0, true, true)]).unwrap(),
            vec![aggregate(Some(0), 0, true, true)]
        );
    }

    #[test]
    fn inconsistent_sequences_are_errors_not_partial_success() {
        for changes in [
            vec![add(1, 1), add(1, 2)],
            vec![remove(1, 1), remove(1, 1)],
            vec![add(1, 1), update(1, 2, 3)],
            vec![remove(1, 1), update(1, 1, 2)],
        ] {
            assert!(matches!(
                transaction_results(changes),
                Err(TransactionResultError::DiscontinuousRow { signature: 1 })
            ));
        }
        assert!(matches!(
            transaction_results(vec![add(1, 1), aggregate(Some(1), 2, false, false)]),
            Err(TransactionResultError::ChangedRowKind { signature: 1 })
        ));
        let mut changed_keys = aggregate(Some(1), 2, false, false);
        if let Context::Aggregation { grouping_keys, .. } = &mut changed_keys {
            grouping_keys.push("value".into());
        }
        assert!(matches!(
            transaction_results(vec![aggregate(None, 1, true, false), changed_keys]),
            Err(TransactionResultError::ChangedGrouping { signature: 1 })
        ));
    }

    #[test]
    fn continuity_handles_nan_without_suppressing_changed_numeric_identity() {
        let nan = QueryVariables::from([("value".into(), VariableValue::Float(f64::NAN.into()))]);
        let result = transaction_results(vec![
            Context::Adding {
                after: nan.clone(),
                row_signature: 1,
            },
            Context::Updating {
                before: nan,
                after: row(2),
                row_signature: 1,
            },
        ])
        .unwrap();
        assert_eq!(result, vec![add(1, 2)]);

        let large = 9_007_199_254_740_993i64;
        let rounded =
            QueryVariables::from([("value".into(), VariableValue::Float((large as f64).into()))]);
        assert_eq!(
            transaction_results(vec![Context::Updating {
                before: row(large),
                after: rounded.clone(),
                row_signature: 1,
            }])
            .unwrap(),
            vec![Context::Updating {
                before: row(large),
                after: rounded,
                row_signature: 1,
            }]
        );
    }
}
