// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Workload placement under policy, health and capacity, with single-loss
//! resilience, approval identities and preconditions. Inputs are the rows that
//! upstream continuous queries produce: one `PlanContext` (run and revision),
//! `PlanItem` (workload demand and current deployment), `PlanBin` (deployment
//! capacity and health), `PlanCandidate` (every workload and deployment pair,
//! with the workload's demand and current deployment and the deployment's
//! capacity) and `PolicyResult` (one policy decision per pair).

use super::*;

fn placement_model() -> Value {
    json!({
        "stream": "plan/out",
        "debounceMs": 0,
        "maxDelayMs": 0,
        "timeLimitMs": 10000,
        "statusLabel": "PlanStatus",
        "scenarioLabel": "LossScenario",
        "sets": [
            {"name": "context", "label": "PlanContext", "key": ["run_epoch"]},
            {"name": "items", "label": "PlanItem", "key": ["id"]},
            {"name": "bins", "label": "PlanBin", "key": ["id"]},
            {"name": "healthy", "label": "PlanBin", "key": ["id"],
             "where": [{"property": "health", "op": "==", "value": "healthy"}]},
            {"name": "unhealthy", "label": "PlanBin", "key": ["id"],
             "where": [{"property": "health", "op": "==", "value": "unhealthy"}]},
            {"name": "decisions", "label": "PolicyResult", "key": ["subject", "target"]},
            {"name": "allowed", "label": "PolicyResult", "key": ["subject", "target"], "where": [
                {"property": "state", "op": "==", "value": "allowed"},
                {"property": "allowed", "op": "==", "value": true}
            ]},
            {"name": "candidates", "label": "PlanCandidate", "key": ["subject", "target"]},
            {"name": "pairs", "label": "PlanCandidate", "key": ["subject", "target"], "where": [
                {"inSet": "allowed", "match": {"subject": "subject", "target": "target"}},
                {"inSet": "healthy", "match": {"target": "id"}},
                {"property": "capacity_tpm", "op": ">=", "valueProperty": "demand_tpm"}
            ]}
        ],
        "variables": [
            {"name": "assign", "type": "binary", "over": "pairs",
             "output": {"label": "Assignment", "properties": ["subject", "target", "current", "demand_tpm"]}},
            {"name": "peak", "type": "continuous", "lower": 0, "output": {"emit": false}}
        ],
        "constraints": [
            {"name": "assign-once", "forEach": "items",
             "terms": [{"variable": "assign", "match": {"subject": "id"}}], "op": "==", "rhs": 1},
            {"name": "capacity", "forEach": "healthy",
             "terms": [{"variable": "assign", "match": {"target": "id"},
                        "coefficient": {"property": "demand_tpm"}}],
             "op": "<=", "rhs": {"property": "capacity_tpm"}},
            {"name": "peak-use", "forEach": "healthy",
             "where": [{"property": "capacity_tpm", "op": ">", "value": 0}],
             "terms": [
                {"variable": "assign", "match": {"target": "id"}, "coefficient": {"property": "demand_tpm"}},
                {"variable": "peak", "coefficient": {"property": "capacity_tpm", "of": "group", "scale": -1}}
             ],
             "op": "<=", "rhs": 0}
        ],
        "objectives": [
            {"name": "moves", "sense": "minimize", "terms": [{"variable": "assign",
                "where": [{"property": "target", "op": "!=", "valueProperty": "current"}]}]},
            {"name": "peak", "sense": "minimize", "terms": [{"variable": "peak"}]}
        ],
        "scenarios": [{"name": "loss", "set": "healthy", "property": "id"}],
        "provenance": {"set": "context", "properties": ["run_epoch", "revision"]},
        "preconditions": [
            {"name": "context-loaded", "set": "context", "min": 1, "max": 1},
            {"name": "items-loaded", "set": "items", "min": 1},
            {"name": "bins-loaded", "set": "bins", "min": 1},
            {"name": "health-known", "set": "bins", "max": 0,
             "where": [{"property": "health", "op": "notIn", "value": ["healthy", "unhealthy"]}]},
            {"name": "policy-known", "set": "decisions", "max": 0,
             "where": [{"property": "state", "op": "notIn", "value": ["allowed", "denied"]}]},
            {"name": "candidates-loaded", "set": "items", "max": 0,
             "where": [{"notInSet": "candidates", "match": {"id": "subject"}}]},
            {"name": "policy-complete", "set": "candidates", "max": 0,
             "where": [{"notInSet": "decisions", "match": {"subject": "subject", "target": "target"}}]},
            {"name": "same-revision", "set": "items", "max": 0,
             "where": [{"notInSet": "context", "match": {"run_epoch": "run_epoch", "revision": "revision"}}]},
            {"name": "same-run", "set": "bins", "max": 0,
             "where": [{"notInSet": "context", "match": {"run_epoch": "run_epoch"}}]},
            {"name": "same-run-policy", "set": "decisions", "max": 0,
             "where": [{"notInSet": "context", "match": {"run_epoch": "run_epoch"}}]}
        ],
        "summaries": [
            {"name": "assignments", "variable": "assign", "properties": ["subject", "target"]},
            {"name": "moved", "variable": "assign", "properties": ["subject", "target"],
             "where": [{"property": "target", "op": "!=", "valueProperty": "current"}]}
        ]
    })
}

#[derive(Clone)]
struct Fleet {
    revision: u64,
    /// (workload, demand, current deployment)
    items: Vec<(&'static str, u64, &'static str)>,
    /// (deployment, capacity, health)
    bins: Vec<(&'static str, u64, &'static str)>,
    /// (workload, deployment, state)
    policy: Vec<(&'static str, &'static str, &'static str)>,
}

impl Fleet {
    /// Two workloads on three 100k deployments; `support` may not leave the EU.
    fn seed() -> Self {
        let mut policy = Vec::new();
        for (subject, allowed) in [
            ("support", ["EU-A", "EU-B"].as_slice()),
            ("batch", ["EU-A", "EU-B", "US-C"].as_slice()),
        ] {
            for target in ["EU-A", "EU-B", "US-C"] {
                let state = if allowed.contains(&target) {
                    "allowed"
                } else {
                    "denied"
                };
                policy.push((subject, target, state));
            }
        }
        Self {
            revision: 1,
            items: vec![("support", 60_000, "EU-A"), ("batch", 80_000, "EU-B")],
            bins: vec![
                ("EU-A", 100_000, "healthy"),
                ("EU-B", 100_000, "healthy"),
                ("US-C", 100_000, "healthy"),
            ],
            policy,
        }
    }

    fn health(&mut self, bin: &str, health: &'static str) -> &mut Self {
        self.bins.iter_mut().find(|b| b.0 == bin).expect("bin").2 = health;
        self
    }

    fn capacity(&mut self, bin: &str, capacity: u64) -> &mut Self {
        self.bins.iter_mut().find(|b| b.0 == bin).expect("bin").1 = capacity;
        self
    }

    fn route(&mut self, item: &str, bin: &'static str) -> &mut Self {
        self.items.iter_mut().find(|i| i.0 == item).expect("item").2 = bin;
        self
    }

    fn decide(&mut self, item: &str, bin: &str, state: &'static str) -> &mut Self {
        self.policy
            .iter_mut()
            .find(|p| p.0 == item && p.1 == bin)
            .expect("decision")
            .2 = state;
        self
    }

    fn records(&self) -> BTreeMap<String, Element> {
        let mut records = BTreeMap::new();
        let mut add = |label: &str, id: String, properties: Value| {
            records.insert(id.clone(), node(label, &id, properties));
        };
        add(
            "PlanContext",
            "context".into(),
            json!({"run_epoch": "run-1", "revision": self.revision}),
        );
        for (id, demand, current) in &self.items {
            add(
                "PlanItem",
                format!("item:{id}"),
                json!({"id": id, "run_epoch": "run-1", "revision": self.revision,
                       "demand_tpm": demand, "deployment": current}),
            );
        }
        for (id, capacity, health) in &self.bins {
            add(
                "PlanBin",
                format!("bin:{id}"),
                json!({"id": id, "run_epoch": "run-1", "capacity_tpm": capacity, "health": health}),
            );
        }
        for (subject, demand, current) in &self.items {
            for (target, capacity, _) in &self.bins {
                add(
                    "PlanCandidate",
                    format!("candidate:{subject}/{target}"),
                    json!({"subject": subject, "target": target, "run_epoch": "run-1",
                           "demand_tpm": demand, "current": current, "capacity_tpm": capacity}),
                );
            }
        }
        for (subject, target, state) in &self.policy {
            add(
                "PolicyResult",
                format!("decision:{subject}/{target}"),
                json!({"subject": subject, "target": target, "state": state,
                       "allowed": *state == "allowed", "run_epoch": "run-1",
                       "reason": "", "policy_hash": "p", "basis": "b"}),
            );
        }
        records
    }
}

/// What downstream queries read from the status row.
#[derive(Debug)]
struct Answer {
    status: String,
    assignments: Vec<(String, String)>,
    moved: Vec<(String, String)>,
    moves: Value,
    peak: Value,
    basis: Value,
    candidate: Value,
    /// (lost deployment, status, detail) for every loss that leaves no plan.
    failures: Vec<(String, String, String)>,
    losses: Value,
    status_row: Value,
    rows: Snapshot,
}

fn pairs(list: &Value) -> Vec<(String, String)> {
    list.as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e["subject"].as_str().unwrap_or_default().to_owned(),
                        e["target"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn answer(rows: Snapshot) -> Answer {
    let status = rows["status"].1.clone();
    let failures = status["scenarios"]["loss"]["failures"]
        .as_array()
        .map(|failures| {
            failures
                .iter()
                .map(|f| {
                    (
                        f["value"].as_str().unwrap_or_default().to_owned(),
                        f["status"].as_str().unwrap_or_default().to_owned(),
                        f["detail"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Answer {
        status: status["status"].as_str().unwrap_or_default().to_owned(),
        assignments: pairs(&status["summaries"]["assignments"]),
        moved: pairs(&status["summaries"]["moved"]),
        moves: status["objectives"]["moves"].clone(),
        peak: status["objectives"]["peak"].clone(),
        basis: status["signature"].clone(),
        candidate: status["solutionSignature"].clone(),
        failures,
        losses: status["scenarios"]["loss"]["values"].clone(),
        status_row: status,
        rows,
    }
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

async fn solve(fleet: &Fleet) -> Result<Answer> {
    let mut optimization = started(placement_model()).await?;
    sync(&mut optimization, &mut BTreeMap::new(), fleet.records()).await?;
    Ok(answer(snapshot(&optimization).await?))
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_seed_needs_no_moves_and_survives_any_single_loss() -> Result<()> {
    let a = solve(&Fleet::seed()).await?;
    assert_eq!(a.status, "optimal", "{:?}", a.status_row);
    assert_eq!(
        a.assignments,
        owned(&[("batch", "EU-B"), ("support", "EU-A")])
    );
    assert!(a.moved.is_empty());
    assert_eq!(a.moves, 0.0);
    assert_eq!(a.peak, 0.8);
    assert_eq!(a.losses, 3);
    assert!(a.failures.is_empty(), "safe: {:?}", a.failures);
    assert_eq!(a.status_row["setSizes"]["unhealthy"], 0);
    assert_eq!(
        a.status_row["provenance"],
        json!({"run_epoch": "run-1", "revision": 1})
    );
    assert_eq!(a.status_row["precondition"], Value::Null);
    assert_eq!(a.status_row["scenariosSignature"], a.basis);
    assert_eq!(a.candidate.as_str().map(str::len), Some(16));
    let assignments = labelled(&a.rows, "Assignment");
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0].1["current"], "EU-B");
    assert_eq!(assignments[0].1["demand_tpm"], 80_000);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_failure_plan_then_policy_edit_is_infeasible() -> Result<()> {
    let mut fleet = Fleet::seed();
    fleet.health("EU-A", "unhealthy");
    let a = solve(&fleet).await?;
    assert_eq!(a.status, "optimal");
    assert_eq!(
        a.assignments,
        owned(&[("batch", "US-C"), ("support", "EU-B")])
    );
    assert_eq!(a.moved, owned(&[("batch", "US-C"), ("support", "EU-B")]));
    assert_eq!(a.moves, 2.0);
    assert_eq!(a.status_row["setSizes"]["unhealthy"], 1);
    // At risk: losing EU-B leaves support nowhere to run.
    assert_eq!(a.losses, 2);
    assert_eq!(a.failures[0].0, "EU-B");
    assert_eq!(a.failures[0].1, "infeasible");
    assert!(
        a.failures[0].2.contains("assign-once[support]"),
        "{}",
        a.failures[0].2
    );
    assert_eq!(a.rows["scenario/loss/EU-B"].1["feasible"], false);

    // The policy edit: support may no longer use EU-B.
    fleet.decide("support", "EU-B", "denied");
    let b = solve(&fleet).await?;
    assert_eq!(b.status, "infeasible");
    assert_eq!(
        b.status_row["detail"],
        "constraint assign-once[support] has no eligible variables and cannot hold"
    );
    assert!(labelled(&b.rows, "Assignment").is_empty());
    assert_eq!(b.candidate, Value::Null);
    assert_eq!(b.status_row["summaries"], Value::Null);
    assert!(!b.failures.is_empty(), "at risk with no baseline plan");
    assert_ne!(b.basis, a.basis);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_capacity_loss_is_reported_until_capacity_returns() -> Result<()> {
    let mut fleet = Fleet::seed();
    fleet
        .route("support", "EU-B")
        .route("batch", "US-C")
        .capacity("EU-A", 50_000);
    let a = solve(&fleet).await?;
    assert_eq!(a.status, "optimal");
    assert_eq!(a.moves, 0.0);
    assert_eq!(a.failures[0].0, "EU-B");
    assert!(a.failures[0].2.contains("[support]"), "{}", a.failures[0].2);
    assert_eq!(a.rows["scenario/loss/EU-A"].1["feasible"], true);

    fleet.capacity("EU-A", 100_000);
    let b = solve(&fleet).await?;
    assert_eq!(b.status, "optimal");
    assert!(b.failures.is_empty(), "safe again: {:?}", b.failures);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_with_one_failure_first_risks_the_next_loss() -> Result<()> {
    let mut fleet = Fleet::seed();
    fleet.health("US-C", "unhealthy");
    let a = solve(&fleet).await?;
    assert_eq!(a.status, "optimal");
    assert_eq!(a.failures[0].0, "EU-A");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_is_unknown_rather_than_guessed_on_incomplete_inputs() -> Result<()> {
    let check = |a: &Answer, precondition: &str| {
        assert_eq!(a.status, "unknown", "{:?}", a.status_row);
        assert_eq!(a.status_row["precondition"], precondition);
        assert!(labelled(&a.rows, "Assignment").is_empty());
        assert!(labelled(&a.rows, "LossScenario").is_empty());
        assert_eq!(a.status_row["scenarios"], Value::Null);
        assert_eq!(a.candidate, Value::Null);
    };
    // An expired health report.
    let mut fleet = Fleet::seed();
    fleet.health("EU-B", "unknown");
    let a = solve(&fleet).await?;
    check(&a, "health-known");
    assert_eq!(
        a.status_row["detail"],
        "precondition health-known does not hold: 1 member of bins matches, expected at most 0: [EU-B]"
    );
    // An undecided policy.
    let mut fleet = Fleet::seed();
    fleet.decide("batch", "US-C", "unknown");
    check(&solve(&fleet).await?, "policy-known");
    // A workload from another revision.
    let mut fleet = Fleet::seed();
    let mut records = fleet.records();
    records.insert(
        "item:batch".into(),
        node(
            "PlanItem",
            "item:batch",
            json!({"id": "batch", "run_epoch": "run-1", "revision": 2,
                   "demand_tpm": 80_000, "deployment": "EU-B"}),
        ),
    );
    let mut optimization = started(placement_model()).await?;
    sync(&mut optimization, &mut BTreeMap::new(), records).await?;
    check(&answer(snapshot(&optimization).await?), "same-revision");
    // One decision missing.
    fleet.policy.retain(|p| !(p.0 == "batch" && p.1 == "US-C"));
    let a = solve(&fleet).await?;
    check(&a, "policy-complete");
    assert!(a.status_row["detail"]
        .as_str()
        .unwrap_or_default()
        .ends_with("[batch, US-C]"));
    // Policy not loaded yet.
    fleet.policy.clear();
    check(&solve(&fleet).await?, "policy-complete");
    // Only the context loaded so far.
    let context: BTreeMap<_, _> = Fleet::seed()
        .records()
        .into_iter()
        .filter(|(id, _)| id == "context")
        .collect();
    let mut optimization = started(placement_model()).await?;
    sync(&mut optimization, &mut BTreeMap::new(), context).await?;
    check(&answer(snapshot(&optimization).await?), "items-loaded");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_unknown_recovers_and_scenarios_return() -> Result<()> {
    let mut fleet = Fleet::seed();
    fleet.health("EU-B", "unknown");
    let mut optimization = started(placement_model()).await?;
    let mut current = BTreeMap::new();
    sync(&mut optimization, &mut current, fleet.records()).await?;
    assert_eq!(answer(snapshot(&optimization).await?).status, "unknown");
    fleet.health("EU-B", "healthy");
    sync(&mut optimization, &mut current, fleet.records()).await?;
    let a = answer(snapshot(&optimization).await?);
    assert_eq!(a.status, "optimal");
    assert_eq!(a.losses, 3);
    assert_eq!(labelled(&a.rows, "LossScenario").len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_identities_detect_stale_approvals() -> Result<()> {
    let mut fleet = Fleet::seed();
    fleet.health("EU-A", "unhealthy");
    let mut optimization = started(placement_model()).await?;
    let mut current = BTreeMap::new();
    sync(&mut optimization, &mut current, fleet.records()).await?;
    let first = answer(snapshot(&optimization).await?);

    // Same facts in another order, on a fresh instance: identical identities.
    let mut reversed = started(placement_model()).await?;
    let changes: Vec<SourceChange> = fleet.records().into_values().rev().map(insert).collect();
    send(&mut reversed, &changes).await?;
    settle(&mut reversed).await?;
    let again = answer(snapshot(&reversed).await?);
    assert_eq!(
        (&again.basis, &again.candidate),
        (&first.basis, &first.candidate)
    );

    // A demand change is a new basis, though the routes are the same.
    fleet.items[1].1 = 80_001;
    sync(&mut optimization, &mut current, fleet.records()).await?;
    let changed = answer(snapshot(&optimization).await?);
    assert_ne!(changed.basis, first.basis);
    assert_eq!(changed.assignments, first.assignments);

    // A new revision is a new basis.
    fleet.items[1].1 = 80_000;
    fleet.revision = 2;
    sync(&mut optimization, &mut current, fleet.records()).await?;
    let revised = answer(snapshot(&optimization).await?);
    assert_ne!(revised.basis, first.basis);
    assert_eq!(revised.status_row["provenance"]["revision"], 2);

    // A different plan has a different identity.
    let mut other = Fleet::seed();
    other
        .health("US-C", "unhealthy")
        .health("EU-B", "unhealthy")
        .capacity("EU-A", 200_000);
    let plan = solve(&other).await?;
    assert_eq!(plan.status, "optimal");
    assert_eq!(
        plan.assignments,
        owned(&[("batch", "EU-A"), ("support", "EU-A")])
    );
    assert_ne!(plan.candidate, first.candidate);
    Ok(())
}

/// Status rows in emitted batches: (signature, scenariosSignature, first failure).
fn published_statuses(batches: &[Vec<SourceChange>]) -> Vec<(Value, Value, Value)> {
    batches
        .iter()
        .flatten()
        .filter_map(|change| match change {
            SourceChange::Insert { element } | SourceChange::Replace { element }
                if element.get_reference().element_id.as_ref() == "status" =>
            {
                let p = element
                    .get_properties()
                    .map_iter(|name, value| (name.to_string(), Value::from(value)))
                    .collect::<serde_json::Map<_, _>>();
                Some((
                    p["signature"].clone(),
                    p["scenariosSignature"].clone(),
                    p["scenarios"]["loss"]["failures"][0]["value"].clone(),
                ))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn atomic_scenarios_publish_each_result_with_its_own_analysis() -> Result<()> {
    let mut model = placement_model();
    model["atomicScenarios"] = json!(true);
    let mut fleet = Fleet::seed();
    fleet
        .route("support", "EU-B")
        .route("batch", "US-C")
        .health("EU-A", "unhealthy");
    let mut optimization = started(model).await?;
    let mut current = BTreeMap::new();
    sync(&mut optimization, &mut current, fleet.records()).await?;
    let degraded = answer(snapshot(&optimization).await?);
    assert_eq!(degraded.failures[0].0, "EU-B");

    // EU-A recovers: the result and its analysis arrive together, never the new
    // result beside the previous analysis (losing EU-B would leave no plan).
    fleet.health("EU-A", "healthy");
    let next = fleet.records();
    let changes: Vec<SourceChange> = next
        .iter()
        .filter(|(id, element)| current.get(*id) != Some(*element))
        .map(|(_, element)| SourceChange::Replace {
            element: element.clone(),
        })
        .collect();
    send(&mut optimization, &changes).await?;
    let batches = settle(&mut optimization).await?;
    let statuses = published_statuses(&batches);
    assert!(!statuses.is_empty());
    for (signature, scenarios, first) in &statuses {
        assert_eq!(
            signature, scenarios,
            "status published beside a stale analysis"
        );
        assert_eq!(*first, Value::Null, "safe once EU-A is back");
    }
    Ok(())
}
