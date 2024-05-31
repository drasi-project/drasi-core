use crate::evaluation::variable_value::VariableValue;
use serde_json::json;

#[test]
fn test_from_string() {
    let value = VariableValue::from("hello");
    assert_eq!(value, VariableValue::String("hello".to_string()));
    assert_eq!("hello", value.as_str().unwrap());
}

#[test]
fn test_from_str() {
    let str_value: &str = "hello";
    let value = VariableValue::from(str_value);

    assert_eq!(value, VariableValue::String("hello".to_string()));
    assert_eq!(str_value, value.as_str().unwrap());
}

#[test]
fn test_is_string() {
    let value = VariableValue::from("hello");
    assert!(value.is_string());
}

#[test]
fn test_string_from_json() {
    let value = VariableValue::from(json!("hello"));
    assert_eq!(value, VariableValue::String("hello".to_string()));
}
