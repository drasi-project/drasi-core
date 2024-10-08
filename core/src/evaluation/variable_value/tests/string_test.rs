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
