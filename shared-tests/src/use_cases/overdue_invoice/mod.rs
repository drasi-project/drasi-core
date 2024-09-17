use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use serde_json::json;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{AutoFutureQueueConsumer, QueryBuilder},
};

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

#[allow(clippy::print_stdout)]
pub async fn overdue_invoice(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let mut builder = QueryBuilder::new(queries::list_overdue_query());
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let inv_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = inv_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //create invoice
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "INV1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "invoiceNumber": "INV1",
                    "invoiceDate": "2020-01-01",
                    "status": "unpaid",
                    "amount": 5.0
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 0);
    }

    //jump to 3 days later
    {
        println!("-----------------later---------------------");
        now += Duration::days(3).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "invoiceNumber" => VariableValue::String("INV1".into()),
                "invoiceDate" => VariableValue::String("2020-01-01".into())
            ),
        }));
    }

    //pay invoice
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "INV1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "status": "paid",
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "invoiceNumber" => VariableValue::String("INV1".into()),
                "invoiceDate" => VariableValue::String("2020-01-01".into())
            ),
        }));
    }
}

#[allow(clippy::print_stdout)]
pub async fn overdue_count_persistent(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let mut builder = QueryBuilder::new(queries::count_overdue_greater_query());
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let inv_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = inv_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //create invoices
    {
        let inv1 = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "INV1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "invoiceNumber": "INV1",
                    "invoiceDate": "2020-01-01",
                    "status": "unpaid",
                    "amount": 5.0
                })),
            },
        };

        let result = cq.process_source_change(inv1.clone()).await.unwrap();
        assert_eq!(result.len(), 0);

        let inv2 = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "INV2"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "invoiceNumber": "INV2",
                    "invoiceDate": "2020-01-01",
                    "status": "unpaid",
                    "amount": 7.0
                })),
            },
        };

        let result = cq.process_source_change(inv2.clone()).await.unwrap();
        assert_eq!(result.len(), 0);

        let inv3 = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "INV3"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "invoiceNumber": "INV3",
                    "invoiceDate": "2020-01-01",
                    "status": "unpaid",
                    "amount": 3.0
                })),
            },
        };

        let result = cq.process_source_change(inv3.clone()).await.unwrap();
        assert_eq!(result.len(), 0);
    }

    //jump to 3 days later
    {
        println!("-----------------later---------------------");
        now += Duration::days(3).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(3)).await;
        assert!(result.is_some());
        let result = result.unwrap();

        assert_eq!(result.len(), 1);
        println!("later: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "count" => VariableValue::Integer(3.into())
            ),
        }));
    }

    //pay invoice
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "INV1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "status": "paid",
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "count" => VariableValue::Integer(3.into())
            ),
        }));
    }
}
