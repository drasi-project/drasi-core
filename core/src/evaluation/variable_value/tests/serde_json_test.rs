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
