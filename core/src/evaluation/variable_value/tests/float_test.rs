use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use serde_json::json;

#[test]
fn test_from_float_f64() {
    let num: f64 = 1234.0;
    let value = VariableValue::from(num);
    assert_eq!(value, VariableValue::Float(1234.0.into()));
    assert_eq!(1234.0, value.as_f64().unwrap());
}

#[test]
fn test_float_is_f64() {
    let num: f64 = 1234.0;
    let value = VariableValue::from(num);
    assert!(value.is_f64());
}

#[test]
fn test_float_input_integer() {
    let num: i64 = 1234;
    let value = VariableValue::Float(Float::from(num));
    assert_eq!(1234.0, value.as_f64().unwrap());
}

#[test]
fn test_float_from_json() {
    let num = json!(1234.0);
    let value = VariableValue::from(num);
    assert_eq!(value, VariableValue::Float(1234.0.into()));

    let integer_val = VariableValue::from(json!(1234));

    assert_eq!(value, integer_val);
}

#[test]
fn test_float_comparison() {
    let value = VariableValue::Float(1234.0.into());
    assert!(value > VariableValue::Float(1000.0.into()));

    let value = VariableValue::Integer(1234.into());
    assert!(value > VariableValue::Float(1000.0.into()));
}
