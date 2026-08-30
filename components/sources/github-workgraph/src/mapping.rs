use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use sha2::{Digest, Sha256};

use crate::agents::{AgentFile, AgentFileContent, AgentFileLocation, MAX_AGENT_SLOTS};
use crate::lease_ledger::{AgentRuntime, AllocationDelta, WorkGraphActiveLease};
use crate::model::{
    agent_config_error_element_id, agent_element_id, agent_slot_element_id, slot_id, WorkGraphError,
};

pub const NODE_LABELS: [&str; 14] = [
    "WorkGraphRootIssue",
    "WorkflowDefinition",
    "TaskDefinition",
    "WorkflowRun",
    "WorkGraphTask",
    "WorkGraphTaskAssign",
    "WorkGraphTaskDispatch",
    "WorkGraphTaskResult",
    "WorkGraphTaskEvaluate",
    "WorkGraphTaskArtifact",
    "WorkGraphTaskLease",
    "WorkGraphAgent",
    "WorkGraphAgentSlot",
    "WorkGraphError",
];

pub const RELATION_LABELS: [&str; 18] = [
    "HAS_ROOT",
    "HAS_TASK",
    "DECLARES_CHILD",
    "USES_DEFINITION",
    "INSTANCE_OF",
    "IN_RUN",
    "TASK_FOR",
    "ROOT_TASK_FOR",
    "RUN_FOR",
    "ASSIGNS",
    "DISPATCHES",
    "RESULT_FOR",
    "RESULT_FROM_LEASE",
    "EVALUATES",
    "ARTIFACT_FOR",
    "HAS_SLOT",
    "LEASE_FOR",
    "LEASES_SLOT",
];

const NODE_WORKGRAPH_TASK_LEASE: &str = "WorkGraphTaskLease";
const NODE_WORKGRAPH_TASK_ARTIFACT: &str = "WorkGraphTaskArtifact";
const NODE_WORKGRAPH_AGENT: &str = "WorkGraphAgent";
const NODE_WORKGRAPH_AGENT_SLOT: &str = "WorkGraphAgentSlot";
const NODE_WORKGRAPH_ERROR: &str = "WorkGraphError";
const REL_HAS_SLOT: &str = "HAS_SLOT";
const REL_LEASE_FOR: &str = "LEASE_FOR";
const REL_LEASES_SLOT: &str = "LEASES_SLOT";
const REL_ARTIFACT_FOR: &str = "ARTIFACT_FOR";

pub enum AgentProjection<'a> {
    Loaded {
        file: &'a AgentFile,
        content: &'a AgentFileContent,
    },
    Rejected(&'a WorkGraphError),
}

pub fn agent_changes(
    source_id: &str,
    effective_from: u64,
    location: &AgentFileLocation,
    projection: &AgentProjection<'_>,
    retiring: &BTreeMap<String, BTreeSet<u32>>,
    removed: &BTreeMap<String, BTreeSet<u32>>,
) -> Vec<SourceChange> {
    let mut changes = Changes::new(source_id, effective_from);
    let error_id = agent_config_error_element_id();

    match projection {
        AgentProjection::Rejected(error) => {
            let mut properties = ElementPropertyMap::new();
            properties.text("errorKind", "invalid-workgraph-agent-config");
            properties.text("errorCode", error.code);
            properties.text("errorMessage", &error.message);
            properties.text("configRepository", &location.repository);
            properties.text("configRef", &location.r#ref);
            properties.text("configPath", &location.path);
            changes.node(Update, &error_id, NODE_WORKGRAPH_ERROR, properties);
            return changes.values;
        }
        AgentProjection::Loaded { file, content } => {
            changes.delete(&error_id, NODE_WORKGRAPH_ERROR);
            for (agent_id, slots) in removed {
                delete_agent(&mut changes, agent_id, slots);
            }
            for agent in &file.agents {
                let agent_element = agent_element_id(&agent.agent_id);
                let mut properties = ElementPropertyMap::new();
                properties.text("agentId", &agent.agent_id);
                properties.insert(
                    "configuredSlotCount",
                    ElementValue::Integer(i64::from(agent.slots)),
                );
                properties.insert("queueDepth", ElementValue::Integer(0));
                properties.insert("activeLeaseCount", ElementValue::Integer(0));
                properties.insert(
                    "availableSlotCount",
                    ElementValue::Integer(i64::from(agent.slots)),
                );
                properties.text("leaseDuration", &agent.lease_duration);
                properties.insert(
                    "leaseDurationSeconds",
                    ElementValue::Integer(agent.lease_duration_seconds),
                );
                properties.insert(
                    "agentFileVersion",
                    ElementValue::Integer(file.version as i64),
                );
                properties.text("configRepository", &location.repository);
                properties.text("configRef", &location.r#ref);
                properties.text("configPath", &location.path);
                properties.text("configBlobOid", &content.oid);
                properties.text("configDigest", &sha256_digest(&content.text));
                changes.node(Update, &agent_element, NODE_WORKGRAPH_AGENT, properties);

                let retiring = retiring.get(&agent.agent_id).cloned().unwrap_or_default();
                for slot_number in (1..=agent.slots).chain(retiring.iter().copied()) {
                    let slot = slot_id(&agent.agent_id, slot_number);
                    let slot_element = agent_slot_element_id(&slot);
                    let enabled = slot_number <= agent.slots;
                    let mut properties = ElementPropertyMap::new();
                    properties.text("slotId", &slot);
                    properties.insert("slotNumber", ElementValue::Integer(i64::from(slot_number)));
                    properties.text("agentId", &agent.agent_id);
                    properties.insert("enabled", ElementValue::Bool(enabled));
                    properties.insert("retiring", ElementValue::Bool(!enabled));
                    properties.text("leaseDuration", &agent.lease_duration);
                    properties.insert(
                        "leaseDurationSeconds",
                        ElementValue::Integer(agent.lease_duration_seconds),
                    );
                    properties.text("configRepository", &location.repository);
                    properties.text("configRef", &location.r#ref);
                    properties.text("configPath", &location.path);
                    changes.node(Update, &slot_element, NODE_WORKGRAPH_AGENT_SLOT, properties);
                    changes.relation(
                        Update,
                        REL_HAS_SLOT,
                        &relation_id(REL_HAS_SLOT, &agent_element, &slot_element),
                        &agent_element,
                        &slot_element,
                    );
                }
            }
        }
    }
    changes.values
}

pub fn allocation_changes(
    source_id: &str,
    effective_from: u64,
    delta: &AllocationDelta,
    runtime: &BTreeMap<String, AgentRuntime>,
) -> Vec<SourceChange> {
    let mut changes = Changes::new(source_id, effective_from);
    for lease in &delta.workgraph_ended {
        delete_workgraph_lease(&mut changes, lease, true);
    }
    for lease in &delta.workgraph_historical_ended {
        delete_workgraph_lease(&mut changes, lease, false);
    }
    for lease in &delta.workgraph_released {
        delete_workgraph_lease_slot(&mut changes, lease);
    }
    for (agent_id, slot_number) in &delta.removed_slots {
        let agent = agent_element_id(agent_id);
        let slot = agent_slot_element_id(&slot_id(agent_id, *slot_number));
        changes.delete(&relation_id(REL_HAS_SLOT, &agent, &slot), REL_HAS_SLOT);
        changes.delete(&slot, NODE_WORKGRAPH_AGENT_SLOT);
    }
    for agent_id in &delta.removed_agents {
        changes.delete(&agent_element_id(agent_id), NODE_WORKGRAPH_AGENT);
    }
    for lease in &delta.workgraph_historical {
        upsert_workgraph_lease(&mut changes, lease, false);
    }
    for lease in &delta.workgraph_started {
        upsert_workgraph_lease(&mut changes, lease, true);
    }
    for agent_id in &delta.affected_agents {
        let Some(agent_runtime) = runtime.get(agent_id) else {
            continue;
        };
        let agent = agent_element_id(agent_id);
        let mut properties = ElementPropertyMap::new();
        properties.insert(
            "configuredSlotCount",
            ElementValue::Integer(i64::from(agent_runtime.configured_slots)),
        );
        properties.insert(
            "queueDepth",
            ElementValue::Integer(agent_runtime.queue_depth as i64),
        );
        properties.insert(
            "activeLeaseCount",
            ElementValue::Integer(agent_runtime.active_lease_count as i64),
        );
        properties.insert(
            "availableSlotCount",
            ElementValue::Integer(agent_runtime.available_slot_count as i64),
        );
        changes.node(Update, &agent, NODE_WORKGRAPH_AGENT, properties);
        for slot_number in &agent_runtime.retiring_slots {
            let slot_id = slot_id(agent_id, *slot_number);
            let slot = agent_slot_element_id(&slot_id);
            let mut properties = ElementPropertyMap::new();
            properties.text("slotId", &slot_id);
            properties.insert("slotNumber", ElementValue::Integer(i64::from(*slot_number)));
            properties.text("agentId", agent_id);
            properties.insert("enabled", ElementValue::Bool(false));
            properties.insert("retiring", ElementValue::Bool(true));
            changes.node(Update, &slot, NODE_WORKGRAPH_AGENT_SLOT, properties);
            changes.relation(
                Update,
                REL_HAS_SLOT,
                &relation_id(REL_HAS_SLOT, &agent, &slot),
                &agent,
                &slot,
            );
        }
    }
    changes.values
}

fn upsert_workgraph_lease(changes: &mut Changes<'_>, lease: &WorkGraphActiveLease, active: bool) {
    let element_id = workgraph_lease_element_id(&lease.lease_id);
    let mut properties = ElementPropertyMap::new();
    properties.text("leaseId", &lease.lease_id);
    properties.text("taskId", &lease.task_id);
    properties.text("assignmentId", &lease.assignment_id);
    properties.text("executorId", &lease.executor_id);
    properties.text("slotId", &lease.slot_id);
    properties.text("acquiredAt", &lease.acquired_at);
    properties.text("expiresAt", &lease.expires_at);
    properties.insert("active", ElementValue::Bool(active));
    properties.insert("hasDispatch", ElementValue::Bool(lease.has_dispatch));
    properties.insert("completed", ElementValue::Bool(lease.completed));
    properties.insert(
        "completionEligible",
        ElementValue::Bool(lease.completion_eligible),
    );
    properties.insert("selected", ElementValue::Bool(active || lease.completed));
    changes.node(Update, &element_id, NODE_WORKGRAPH_TASK_LEASE, properties);
    for (artifact_name, artifact_id) in workgraph_lease_artifact_details(lease) {
        let artifact_element = workgraph_lease_artifact_element_id(&lease.lease_id, artifact_name);
        let mut properties = ElementPropertyMap::new();
        properties.text("taskId", &lease.task_id);
        properties.text("leaseId", &lease.lease_id);
        properties.text("artifactName", artifact_name);
        properties.text("artifactId", artifact_id);
        properties.insert("leaseActive", ElementValue::Bool(active));
        properties.insert("leaseCompleted", ElementValue::Bool(lease.completed));
        changes.node(
            Update,
            &artifact_element,
            NODE_WORKGRAPH_TASK_ARTIFACT,
            properties,
        );
        changes.relation(
            Update,
            REL_ARTIFACT_FOR,
            &relation_id(REL_ARTIFACT_FOR, &artifact_element, &lease.task_element_id),
            &artifact_element,
            &lease.task_element_id,
        );
    }
    changes.relation(
        Update,
        REL_LEASE_FOR,
        &relation_id(REL_LEASE_FOR, &element_id, &lease.task_element_id),
        &element_id,
        &lease.task_element_id,
    );
    if active {
        let slot = agent_slot_element_id(&lease.slot_id);
        changes.relation(
            Update,
            REL_LEASES_SLOT,
            &relation_id(REL_LEASES_SLOT, &element_id, &slot),
            &element_id,
            &slot,
        );
    }
}

fn delete_workgraph_lease(changes: &mut Changes<'_>, lease: &WorkGraphActiveLease, active: bool) {
    let element_id = workgraph_lease_element_id(&lease.lease_id);
    for (artifact_name, _) in workgraph_lease_artifact_details(lease) {
        let artifact_element = workgraph_lease_artifact_element_id(&lease.lease_id, artifact_name);
        changes.delete(
            &relation_id(REL_ARTIFACT_FOR, &artifact_element, &lease.task_element_id),
            REL_ARTIFACT_FOR,
        );
        changes.delete(&artifact_element, NODE_WORKGRAPH_TASK_ARTIFACT);
    }
    changes.delete(
        &relation_id(REL_LEASE_FOR, &element_id, &lease.task_element_id),
        REL_LEASE_FOR,
    );
    if active {
        changes.delete(
            &relation_id(
                REL_LEASES_SLOT,
                &element_id,
                &agent_slot_element_id(&lease.slot_id),
            ),
            REL_LEASES_SLOT,
        );
    }
    changes.delete(&element_id, NODE_WORKGRAPH_TASK_LEASE);
}

fn delete_workgraph_lease_slot(changes: &mut Changes<'_>, lease: &WorkGraphActiveLease) {
    let element_id = workgraph_lease_element_id(&lease.lease_id);
    changes.delete(
        &relation_id(
            REL_LEASES_SLOT,
            &element_id,
            &agent_slot_element_id(&lease.slot_id),
        ),
        REL_LEASES_SLOT,
    );
}

fn workgraph_lease_artifact_details(lease: &WorkGraphActiveLease) -> [(&'static str, &str); 4] {
    [
        ("lease.id", &lease.lease_id),
        ("lease.assignmentId", &lease.assignment_id),
        ("lease.executorId", &lease.executor_id),
        ("lease.slotId", &lease.slot_id),
    ]
}

pub fn workgraph_lease_element_id(lease_id: &str) -> String {
    format!("workgraph-v1:lease:{lease_id}")
}

fn workgraph_lease_artifact_element_id(lease_id: &str, artifact_name: &str) -> String {
    format!("workgraph-v1:artifact:{lease_id}:{artifact_name}")
}

fn delete_agent(changes: &mut Changes<'_>, agent_id: &str, slots: &BTreeSet<u32>) {
    let agent_element = agent_element_id(agent_id);
    for slot_number in slots
        .iter()
        .copied()
        .filter(|slot| *slot <= MAX_AGENT_SLOTS)
    {
        let slot_element = agent_slot_element_id(&slot_id(agent_id, slot_number));
        changes.delete(
            &relation_id(REL_HAS_SLOT, &agent_element, &slot_element),
            REL_HAS_SLOT,
        );
        changes.delete(&slot_element, NODE_WORKGRAPH_AGENT_SLOT);
    }
    changes.delete(&agent_element, NODE_WORKGRAPH_AGENT);
}

fn sha256_digest(body: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(body)))
}

fn relation_id(label: &str, from: &str, to: &str) -> String {
    format!("workgraph-v1:{label}:{from}:{to}")
}

trait Properties {
    fn text(&mut self, key: &str, value: &str);
}

impl Properties for ElementPropertyMap {
    fn text(&mut self, key: &str, value: &str) {
        self.insert(key, ElementValue::String(Arc::from(value)));
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Update,
}

use Operation::Update;

struct Changes<'a> {
    source_id: &'a str,
    effective_from: u64,
    values: Vec<SourceChange>,
}

impl<'a> Changes<'a> {
    fn new(source_id: &'a str, effective_from: u64) -> Self {
        Self {
            source_id,
            effective_from,
            values: Vec::new(),
        }
    }

    fn metadata(&self, id: &str, label: &str) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new(self.source_id, id),
            labels: Arc::from(vec![Arc::from(label)]),
            effective_from: self.effective_from,
        }
    }

    fn node(
        &mut self,
        _operation: Operation,
        id: &str,
        label: &str,
        properties: ElementPropertyMap,
    ) {
        self.values.push(SourceChange::Update {
            element: Element::Node {
                metadata: self.metadata(id, label),
                properties,
            },
        });
    }

    fn relation(&mut self, _operation: Operation, label: &str, id: &str, from: &str, to: &str) {
        self.values.push(SourceChange::Update {
            element: Element::Relation {
                metadata: self.metadata(id, label),
                in_node: ElementReference::new(self.source_id, from),
                out_node: ElementReference::new(self.source_id, to),
                properties: ElementPropertyMap::new(),
            },
        });
    }

    fn delete(&mut self, id: &str, label: &str) {
        self.values.push(SourceChange::Delete {
            metadata: self.metadata(id, label),
        });
    }
}
