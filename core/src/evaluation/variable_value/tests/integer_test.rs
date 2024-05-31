use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use serde_json::json;

#[test]
fn test_from_integer_i64() {
    let num: i64 = 1234;
    let value = VariableValue::from(num);
    assert_eq!(value, VariableValue::Integer(1234.into()));
    assert_eq!(1234, value.as_i64().unwrap());
}

#[test]
fn test_from_integer_u64() {
    let num: u64 = 1234;
    let value = VariableValue::from(num);
    assert_eq!(value, VariableValue::Integer(1234.into()));
    assert_eq!(1234, value.as_u64().unwrap());
}

#[test]
fn test_from_usize() {
    let num: usize = 1234;
    let value = VariableValue::from(num);
    assert_eq!(value, VariableValue::Integer(1234.into()));
    assert_eq!(1234, value.as_u64().unwrap());
}

#[test]
fn test_integer_is_i64() {
    let num: i64 = 1234;
    let value = VariableValue::from(num);
    assert!(value.is_i64());
}

#[test]
fn test_integer_float_input() {
    let num: f64 = 1234.0;
    let value = VariableValue::Integer(num.into());
    assert_eq!(1234, value.as_i64().unwrap());
    let num: f64 = 1234.1;
    assert!(std::panic::catch_unwind(|| VariableValue::Integer(num.into())).is_err());
}

#[test]
fn test_integer_float_eq() {
    let num: f64 = 1.0;
    let value = VariableValue::Integer(1.into());
    assert_eq!(
        value,
        VariableValue::Float(Float::from_f64(num).unwrap())
            .as_f64()
            .unwrap()
    );
}

#[test]
fn test_integer_from_json() {
    let num = json!(1234);
    let value = VariableValue::from(num);
    assert_eq!(value, VariableValue::Integer(1234.into()));

    let float_value = VariableValue::Float(Float::from_f64(1234.0).unwrap());

    assert_eq!(value, float_value);
}

#[test]
fn test_integer_comparison() {
    let value = VariableValue::Integer(1234.into());
    assert!(value > VariableValue::Integer(1000.into()));
}
