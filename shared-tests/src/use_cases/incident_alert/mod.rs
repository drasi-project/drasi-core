use std::{sync::Arc, time::SystemTime};

use serde_json::json;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

use self::data::get_bootstrap_data;

use crate::QueryTestConfig;

mod data;
mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

async fn bootstrap_query(query: &ContinuousQuery) {
    let data = get_bootstrap_data();

    for change in data {
        let _ = query.process_source_change(change).await;
    }
}

#[allow(clippy::print_stdout)]
pub async fn incident_alert(config: &(impl QueryTestConfig + Send)) {
    let manager_incident_alert_query = {
        let mut builder = QueryBuilder::new(queries::manager_incident_alert_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let employee_incident_alert_query = {
        let mut builder = QueryBuilder::new(queries::employee_incident_alert_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let employees_at_risk_count_query = {
        let mut builder = QueryBuilder::new(queries::employees_at_risk_count_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    bootstrap_query(&manager_incident_alert_query).await;
    bootstrap_query(&employee_incident_alert_query).await;
    bootstrap_query(&employees_at_risk_count_query).await;

    //Add new high severity fire Incident node
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.HumanResources", "in1000"),
                    labels: Arc::new([Arc::from("Incident")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "type": "environmental", "severity": "high", "description": "Forest Fire", "endTimeMs": null }),
                ),
            },
        };

        assert_eq!(
            manager_incident_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
        assert_eq!(
            employee_incident_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
        assert_eq!(
            employees_at_risk_count_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
    }

    //Connect new high severity fire Incident node
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.HumanResources", "r13"),
                    labels: Arc::new([Arc::from("OCCURS_IN")]),
                    effective_from: 1100,
                },
                properties: ElementPropertyMap::from(json!({})),
                in_node: ElementReference::new("Contoso.HumanResources", "in1000"),
                out_node: ElementReference::new("Contoso.HumanResources", "socal"),
            },
        };

        assert_eq!(
            manager_incident_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
        assert_eq!(
            employee_incident_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
        assert_eq!(
            employees_at_risk_count_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
    }

    //Incident becomes extreme severity
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.HumanResources", "in1000"),
                    labels: Arc::new([Arc::from("Incident")]),
                    effective_from: 1200,
                },
                properties: ElementPropertyMap::from(
                    json!({ "type": "environmental", "severity": "extreme", "description": "Forest Fire", "endTimeMs": null }),
                ),
            },
        };

        let result = manager_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        let result = employee_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Allen")),
              "EmployeeEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        let result = employees_at_risk_count_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            default_before: false,
            default_after: false,
            grouping_keys: vec![
                "RegionName".into(),
                "IncidentId".into(),
                "IncidentSeverity".into(),
                "IncidentDescription".into()
            ],
            before: None,
            after: variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme")),
              "EmployeeCount" => VariableValue::from(json!(3))
            ),
        }));
    }

    //Remove LOCATED_IN relation between danny and b040
    {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("Contoso.HumanResources", "rg09"),
                effective_from: 1300,
                labels: Arc::new([Arc::from("LOCATED_IN")]),
            },
        };

        assert_eq!(
            manager_incident_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
        assert_eq!(
            employee_incident_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
        assert_eq!(
            employees_at_risk_count_query
                .process_source_change(change.clone())
                .await
                .unwrap(),
            vec![]
        );
    }

    //Add LOCATED_IN relation between danny and mv001
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.HumanResources", "rg14"),
                    labels: Arc::new([Arc::from("LOCATED_IN")]),
                    effective_from: 1400,
                },
                properties: ElementPropertyMap::from(json!({})),
                in_node: ElementReference::new("Contoso.HumanResources", "danny"),
                out_node: ElementReference::new("Contoso.HumanResources", "mv001"),
            },
        };

        let result = manager_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        let result = employee_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
        }));

        let result = employees_at_risk_count_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("getting result {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            default_before: true,
            default_after: false,
            grouping_keys: vec![
                "RegionName".into(),
                "IncidentId".into(),
                "IncidentSeverity".into(),
                "IncidentDescription".into()
            ],
            before: Some(variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme")),
              "EmployeeCount" => VariableValue::from(json!(3))
            )),
            after: variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme")),
              "EmployeeCount" => VariableValue::from(json!(4))
            ),
        }));
    }

    //Incident becomes critical severity
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.HumanResources", "in1000"),
                    labels: Arc::new([Arc::from("Incident")]),
                    effective_from: 1500,
                },
                properties: ElementPropertyMap::from(
                    json!({ "type": "environmental", "severity": "critical", "description": "Forest Fire", "endTimeMs": null }),
                ),
            },
        };

        let result = manager_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        let result = employee_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 4);
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Allen")),
              "EmployeeEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("extreme"))
            ),
            after: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Allen")),
              "EmployeeEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        let result = employees_at_risk_count_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        println!("getting result {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            default_before: false,
            default_after: false,
            grouping_keys: vec![
                "RegionName".into(),
                "IncidentId".into(),
                "IncidentSeverity".into(),
                "IncidentDescription".into()
            ],
            before: Some(variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical")),
              "EmployeeCount" => VariableValue::from(json!(0))
            )),
            after: variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical")),
              "EmployeeCount" => VariableValue::from(json!(4))
            ),
        }));
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            default_before: false,
            default_after: false,
            grouping_keys: vec![
                "RegionName".into(),
                "IncidentId".into(),
                "IncidentSeverity".into(),
                "IncidentDescription".into()
            ],
            before: Some(variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical")),
              "EmployeeCount" => VariableValue::from(json!(0))
            )),
            after: variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical")),
              "EmployeeCount" => VariableValue::from(json!(4))
            ),
        }));
    }

    //Resolve Incident
    {
        //get the current time in milliseconds
        let time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.HumanResources", "in1000"),
                    labels: Arc::new([Arc::from("Incident")]),
                    effective_from: 1600,
                },
                properties: ElementPropertyMap::from(
                    json!({ "type": "environmental", "severity": "critical", "description": "Forest Fire", "endTimeMs": time }),
                ),
            },
        };

        let result = manager_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "ManagerName" => VariableValue::from(json!("Allen")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "ManagerEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        let result = employee_incident_alert_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 4);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Claire")),
              "EmployeeEmail" => VariableValue::from(json!("claire@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Bob")),
              "EmployeeEmail" => VariableValue::from(json!("bob@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Danny")),
              "EmployeeEmail" => VariableValue::from(json!("danny@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "EmployeeName" => VariableValue::from(json!("Allen")),
              "EmployeeEmail" => VariableValue::from(json!("allen@contoso.com")),
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical"))
            ),
        }));

        let result = employees_at_risk_count_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            default_before: false,
            default_after: false,
            grouping_keys: vec![
                "RegionName".into(),
                "IncidentId".into(),
                "IncidentSeverity".into(),
                "IncidentDescription".into()
            ],
            before: Some(variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical")),
              "EmployeeCount" => VariableValue::from(json!(4))
            )),
            after: variablemap!(
              "IncidentId" => VariableValue::from(json!("in1000")),
              "IncidentDescription" => VariableValue::from(json!("Forest Fire")),
              "RegionName" => VariableValue::from(json!("SoCal")),
              "IncidentSeverity" => VariableValue::from(json!("critical")),
              "EmployeeCount" => VariableValue::from(json!(0))
            ),
        }));
    }
}
