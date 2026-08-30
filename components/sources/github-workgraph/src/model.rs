#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGraphError {
    pub code: &'static str,
    pub message: String,
}

impl WorkGraphError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub fn agent_config_error_element_id() -> String {
    "workgraph-v1:error:agent-config".to_string()
}

pub fn agent_element_id(agent_id: &str) -> String {
    format!("workgraph-v1:agent:{agent_id}")
}

pub fn slot_id(agent_id: &str, slot_number: u32) -> String {
    format!("{agent_id}/{slot_number}")
}

pub fn agent_slot_element_id(slot_id: &str) -> String {
    format!("workgraph-v1:agent-slot:{slot_id}")
}
