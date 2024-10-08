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
