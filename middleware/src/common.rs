use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ErrorHandling {
    #[default]
    Fail,
    Skip,
}