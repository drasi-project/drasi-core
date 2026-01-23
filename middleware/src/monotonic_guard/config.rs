use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct MonotonicGuardConfig {
    pub timestamp_property: String,
}
