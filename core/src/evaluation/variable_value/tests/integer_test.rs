// Copyright 2024 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

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
