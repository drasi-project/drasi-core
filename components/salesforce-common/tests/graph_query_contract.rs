use anyhow::{anyhow, Result};
use drasi_core::models::{ElementValue, SourceChange};
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_salesforce_common::{graph_changes, Catalog, Describe, DescribeField, Record};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

const SOURCE_ID: &str = "salesforce-source";
const ORG_ID: &str = "00D000000000001AAA";
const QUERY_ID: &str = "contact-account";

#[tokio::test]
async fn graph_changes_match_core_relation_contract() -> Result<()> {
    let catalog = Catalog::from_describes(vec![account_describe(), contact_describe()])?;

    let (source, handle) = ApplicationSource::new(
        SOURCE_ID,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )?;
    let query = Query::cypher(QUERY_ID)
        .query(
            "MATCH (c:Contact)-[:Account]->(a:Account) RETURN c.Name AS contact, a.Name AS account",
        )
        .from_source(SOURCE_ID)
        .auto_start(true)
        .build();
    let (reaction, reaction_handle) = ApplicationReaction::builder("results")
        .with_query(QUERY_ID)
        .build();
    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;
    drasi.start().await?;

    let mut subscription = reaction_handle
        .subscribe_with_options(Default::default())
        .await?;

    let account_a = catalog.record(
        "Account",
        Record {
            id: "001000000000001AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Acme".into()))]),
            timestamp: 1_700_000_000_010,
        },
    )?;
    let account_a_reinserted = catalog.record(
        "Account",
        Record {
            id: "001000000000001AAA".to_string(),
            properties: BTreeMap::from([(
                "Name".to_string(),
                ElementValue::String("Acme Reinserted".into()),
            )]),
            timestamp: 1_700_000_000_020,
        },
    )?;
    let account_b = catalog.record(
        "Account",
        Record {
            id: "001000000000002AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Beta".into()))]),
            timestamp: 1_700_000_000_030,
        },
    )?;

    let contact_to_a = catalog.record(
        "Contact",
        Record {
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                (
                    "AccountId".to_string(),
                    ElementValue::String("001000000000001AAA".into()),
                ),
            ]),
            timestamp: 1_700_000_000_040,
        },
    )?;
    let contact_to_b = catalog.record(
        "Contact",
        Record {
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                (
                    "AccountId".to_string(),
                    ElementValue::String("001000000000002AAA".into()),
                ),
            ]),
            timestamp: 1_700_000_000_050,
        },
    )?;
    let contact_without_relation = catalog.record(
        "Contact",
        Record {
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                ("AccountId".to_string(), ElementValue::Null),
            ]),
            timestamp: 1_700_000_000_060,
        },
    )?;
    let contact_without_name = catalog.record(
        "Contact",
        Record {
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([(
                "AccountId".to_string(),
                ElementValue::String("001000000000002AAA".into()),
            )]),
            timestamp: 1_700_000_000_070,
        },
    )?;

    // Relation before target: no joined result should appear until target node exists.
    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            None,
            Some(&contact_to_a),
            contact_to_a.observed_at_ms,
        )?,
    )
    .await?;
    assert_no_change(&mut subscription, Duration::from_millis(600), |entry| {
        matches_change(entry, "ADD", Some("Ada"), Some("Acme"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            None,
            Some(&account_a),
            account_a.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "ADD", Some("Ada"), Some("Acme"))
    })
    .await?;

    // Target delete/reinsert.
    send_changes(
        &handle,
        graph_changes(SOURCE_ID, ORG_ID, Some(&account_a), None, 1_700_000_000_080)?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "DELETE", Some("Ada"), Some("Acme"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            None,
            Some(&account_a_reinserted),
            account_a_reinserted.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "ADD", Some("Ada"), Some("Acme Reinserted"))
    })
    .await?;

    // Reparent/null/owner deletion and sparse property removal behavior.
    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            None,
            Some(&account_b),
            account_b.observed_at_ms,
        )?,
    )
    .await?;
    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            Some(&contact_to_a),
            Some(&contact_to_b),
            contact_to_b.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "DELETE", Some("Ada"), Some("Acme Reinserted"))
    })
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "ADD", Some("Ada"), Some("Beta"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            Some(&contact_to_b),
            Some(&contact_without_relation),
            contact_without_relation.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "DELETE", Some("Ada"), Some("Beta"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            Some(&contact_without_relation),
            Some(&contact_to_b),
            contact_to_b.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "ADD", Some("Ada"), Some("Beta"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            Some(&contact_to_b),
            Some(&contact_without_name),
            contact_without_name.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_update(entry, Some("Ada"), Some("Beta"), None, Some("Beta"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            Some(&contact_without_name),
            None,
            1_700_000_000_090,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "DELETE", None, Some("Beta"))
    })
    .await?;

    drasi.stop().await?;
    Ok(())
}

#[tokio::test]
async fn owner_property_update_with_retained_relation_does_not_duplicate_join_rows() -> Result<()> {
    let catalog = Catalog::from_describes(vec![account_describe(), contact_describe()])?;

    let (source, handle) = ApplicationSource::new(
        SOURCE_ID,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )?;
    let query = Query::cypher(QUERY_ID)
        .query(
            "MATCH (c:Contact)-[:Account]->(a:Account) RETURN c.Name AS contact, a.Name AS account",
        )
        .from_source(SOURCE_ID)
        .auto_start(true)
        .build();
    let (reaction, reaction_handle) = ApplicationReaction::builder("results")
        .with_query(QUERY_ID)
        .build();
    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;
    drasi.start().await?;

    let mut subscription = reaction_handle
        .subscribe_with_options(Default::default())
        .await?;

    let account = catalog.record(
        "Account",
        Record {
            id: "001000000000001AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Acme".into()))]),
            timestamp: 1_700_000_001_000,
        },
    )?;
    let contact_initial = catalog.record(
        "Contact",
        Record {
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                (
                    "AccountId".to_string(),
                    ElementValue::String("001000000000001AAA".into()),
                ),
            ]),
            timestamp: 1_700_000_001_010,
        },
    )?;
    let contact_renamed = catalog.record(
        "Contact",
        Record {
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                (
                    "Name".to_string(),
                    ElementValue::String("Ada Lovelace".into()),
                ),
                (
                    "AccountId".to_string(),
                    ElementValue::String("001000000000001AAA".into()),
                ),
            ]),
            timestamp: 1_700_000_001_020,
        },
    )?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            None,
            Some(&account),
            account.observed_at_ms,
        )?,
    )
    .await?;
    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            None,
            Some(&contact_initial),
            contact_initial.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_change(entry, "ADD", Some("Ada"), Some("Acme"))
    })
    .await?;

    send_changes(
        &handle,
        graph_changes(
            SOURCE_ID,
            ORG_ID,
            Some(&contact_initial),
            Some(&contact_renamed),
            contact_renamed.observed_at_ms,
        )?,
    )
    .await?;
    wait_for_change(&mut subscription, Duration::from_secs(10), |entry| {
        matches_update(
            entry,
            Some("Ada"),
            Some("Acme"),
            Some("Ada Lovelace"),
            Some("Acme"),
        )
    })
    .await?;
    assert_no_change(&mut subscription, Duration::from_millis(700), |entry| {
        matches_change(entry, "ADD", Some("Ada Lovelace"), Some("Acme"))
            || matches_change(entry, "DELETE", Some("Ada Lovelace"), Some("Acme"))
    })
    .await?;

    drasi.stop().await?;
    Ok(())
}

async fn send_changes(
    handle: &drasi_source_application::ApplicationSourceHandle,
    changes: Vec<SourceChange>,
) -> Result<()> {
    for change in changes {
        handle.send(change).await?;
    }
    Ok(())
}

fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn matches_field(data: &Value, field: &str, expected: Option<&str>) -> bool {
    let Some(value) = data.get(field) else {
        return false;
    };
    match expected {
        Some(expected) => as_string(value).is_some_and(|value| value == expected),
        None => value.is_null(),
    }
}

fn matches_change(
    entry: &ResultDiff,
    kind: &str,
    contact: Option<&str>,
    account: Option<&str>,
) -> bool {
    let data = match (kind, entry) {
        ("ADD", ResultDiff::Add { data, .. }) => data,
        ("DELETE", ResultDiff::Delete { data, .. }) => data,
        _ => return false,
    };
    matches_field(data, "contact", contact) && matches_field(data, "account", account)
}

fn matches_update(
    entry: &ResultDiff,
    before_contact: Option<&str>,
    before_account: Option<&str>,
    after_contact: Option<&str>,
    after_account: Option<&str>,
) -> bool {
    let ResultDiff::Update { before, after, .. } = entry else {
        return false;
    };
    matches_field(before, "contact", before_contact)
        && matches_field(before, "account", before_account)
        && matches_field(after, "contact", after_contact)
        && matches_field(after, "account", after_account)
}

async fn wait_for_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    timeout: Duration,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    let started = Instant::now();
    let mut last_results = Vec::new();
    while started.elapsed() < timeout {
        if let Some(result) = subscription.recv().await {
            last_results = result.results.clone();
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }
    Err(anyhow!(
        "timed out waiting for expected query change. last results: {last_results:?}"
    ))
}

async fn assert_no_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    timeout: Duration,
    mut matcher: F,
) -> Result<()>
where
    F: FnMut(&ResultDiff) -> bool,
{
    let started = Instant::now();
    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        let slice = remaining.min(Duration::from_millis(100));
        if let Ok(Some(result)) = tokio::time::timeout(slice, subscription.recv()).await {
            if result.results.iter().any(&mut matcher) {
                return Err(anyhow!(
                    "received an unexpected query change while asserting absence: {:?}",
                    result.results
                ));
            }
        }
    }
    Ok(())
}

fn account_describe() -> Describe {
    Describe {
        name: "Account".to_string(),
        queryable: true,
        key_prefix: "001".to_string(),
        fields: vec![
            describe_field("Id", "id", false),
            describe_field("Name", "string", true).with_compound("Name"),
            describe_field("ParentId", "reference", true)
                .with_relationship("Parent")
                .with_reference_to(vec!["Account"]),
            describe_field("LastModifiedDate", "datetime", false),
        ],
    }
}

fn contact_describe() -> Describe {
    Describe {
        name: "Contact".to_string(),
        queryable: true,
        key_prefix: "003".to_string(),
        fields: vec![
            describe_field("Id", "id", false),
            describe_field("Name", "string", true).with_compound("Name"),
            describe_field("AccountId", "reference", true)
                .with_relationship("Account")
                .with_reference_to(vec!["Account"]),
            describe_field("ReportsToId", "reference", true)
                .with_relationship("ReportsTo")
                .with_reference_to(vec!["Contact"]),
            describe_field("LastModifiedDate", "datetime", false),
        ],
    }
}

trait DescribeFieldExt {
    fn with_relationship(self, label: &str) -> Self;
    fn with_reference_to(self, targets: Vec<&str>) -> Self;
    fn with_compound(self, compound: &str) -> Self;
}

impl DescribeFieldExt for DescribeField {
    fn with_relationship(mut self, label: &str) -> Self {
        self.relationship_name = Some(label.to_string());
        self
    }

    fn with_reference_to(mut self, targets: Vec<&str>) -> Self {
        self.reference_to = targets.into_iter().map(str::to_string).collect();
        self
    }

    fn with_compound(mut self, compound: &str) -> Self {
        self.compound_field_name = Some(compound.to_string());
        self
    }
}

fn describe_field(name: &str, field_type: &str, nillable: bool) -> DescribeField {
    DescribeField {
        name: name.to_string(),
        field_type: field_type.to_string(),
        queryable: true,
        nillable,
        relationship_name: None,
        reference_to: Vec::new(),
        deprecated_and_hidden: false,
        compound_field_name: None,
    }
}
