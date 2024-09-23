use crate::evaluation::variable_value::VariableValue;
use serde_json::json;
// use query_host::api::{ResultEvent, ChangeEvent};

#[test]
fn test_from_slice() {
    let payload = b"{\"after\":{\"id\":\"public:Item:1\",\"labels\":[\"Item\"]}}";

    let value: Result<VariableValue, serde_json::Error> = serde_json::from_slice(payload);
    assert!(value.is_ok());

    let value = value.unwrap();
    assert_eq!(
        value,
        VariableValue::from(json!({"after":{"id":"public:Item:1","labels":["Item"]}}))
    );
}
