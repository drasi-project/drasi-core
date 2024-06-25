use std::sync::Arc;

use drasi_core::{
    evaluation::context::QueryPartEvaluationContext,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use serde_json::json;
use sysinfo::System;

#[tokio::main]
async fn main() {
    let query_str = "
    MATCH 
        (p:Process)
    WHERE p.cpu_usage > 10
    RETURN 
        p.pid AS process_pid,
        p.name AS process_name, 
        p.cpu_usage AS process_cpu_usage
    ";

    let query_builder = QueryBuilder::new(query_str);
    let query = query_builder.build().await;

    let mut sys = System::new_all();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                for source_change in sync_data(&mut sys) {
                    process_change(&query, source_change).await;
                }
            }
        }
    }
}

async fn process_change(query: &ContinuousQuery, change: SourceChange) {
    let result = query.process_source_change(change).await.unwrap();
    for context in result {
        match context {
            QueryPartEvaluationContext::Adding { after } => {
                println!("Adding: {:?}", after);
            }
            QueryPartEvaluationContext::Removing { before } => {
                println!("Removing: {:?}", before);
            }
            QueryPartEvaluationContext::Updating { before, after } => {
                println!("Updating: {:?} -> {:?}", before, after);
            }
            _ => {}
        }
    }
}

fn sync_data(sys: &mut System) -> Vec<SourceChange> {
    sys.refresh_processes();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let result = sys
        .processes()
        .iter()
        .map(|(pid, process)| SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("System", &pid.to_string()),
                    labels: Arc::new([Arc::from("Process")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "pid": pid.to_string(),
                    "name": process.name(),
                    "cpu_usage": process.cpu_usage()
                })),
            },
        })
        .collect();

    result
}
