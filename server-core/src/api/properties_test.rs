// Copyright 2025 The Drasi Authors.
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

//! Tests for Properties builder

#[cfg(test)]
mod tests {
    use crate::api::Properties;
    use serde_json::{json, Value};
    use std::collections::HashMap;

    #[test]
    fn test_properties_new() {
        let props = Properties::new();
        let map = props.build();

        assert!(map.is_empty(), "New Properties should be empty");
    }

    #[test]
    fn test_properties_with_string() {
        let props = Properties::new()
            .with_string("host", "localhost")
            .with_string("database", "mydb");

        let map = props.build();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("host").unwrap(),
            &Value::String("localhost".to_string())
        );
        assert_eq!(
            map.get("database").unwrap(),
            &Value::String("mydb".to_string())
        );
    }

    #[test]
    fn test_properties_with_int() {
        let props = Properties::new()
            .with_int("port", 5432)
            .with_int("timeout", 30);

        let map = props.build();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("port").unwrap(), &json!(5432));
        assert_eq!(map.get("timeout").unwrap(), &json!(30));
    }

    #[test]
    fn test_properties_with_bool() {
        let props = Properties::new()
            .with_bool("ssl", true)
            .with_bool("verify", false);

        let map = props.build();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("ssl").unwrap(), &Value::Bool(true));
        assert_eq!(map.get("verify").unwrap(), &Value::Bool(false));
    }

    #[test]
    fn test_properties_with_value() {
        let props = Properties::new()
            .with_value("config", json!({"nested": "value"}))
            .with_value("array", json!([1, 2, 3]));

        let map = props.build();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("config").unwrap(), &json!({"nested": "value"}));
        assert_eq!(map.get("array").unwrap(), &json!([1, 2, 3]));
    }

    #[test]
    fn test_properties_mixed_types() {
        let props = Properties::new()
            .with_string("host", "localhost")
            .with_int("port", 5432)
            .with_bool("ssl", true)
            .with_value("options", json!({"timeout": 30}));

        let map = props.build();
        assert_eq!(map.len(), 4);
        assert_eq!(map.get("host").unwrap(), &json!("localhost"));
        assert_eq!(map.get("port").unwrap(), &json!(5432));
        assert_eq!(map.get("ssl").unwrap(), &json!(true));
        assert_eq!(map.get("options").unwrap(), &json!({"timeout": 30}));
    }

    #[test]
    fn test_properties_chaining() {
        let map = Properties::new()
            .with_string("a", "1")
            .with_int("b", 2)
            .with_bool("c", true)
            .build();

        assert_eq!(map.len(), 3);
    }


    #[test]
    fn test_properties_from_map() {
        let mut original = HashMap::new();
        original.insert("key1".to_string(), json!("value1"));
        original.insert("key2".to_string(), json!(42));

        let props = Properties::from_map(original.clone());
        let map = props.build();

        assert_eq!(map.len(), 2);
        assert_eq!(map.get("key1").unwrap(), &json!("value1"));
        assert_eq!(map.get("key2").unwrap(), &json!(42));
    }

    #[test]
    fn test_properties_from_impl() {
        let mut original = HashMap::new();
        original.insert("key".to_string(), json!("value"));

        let props: Properties = original.clone().into();
        let map = props.build();

        assert_eq!(map.get("key").unwrap(), &json!("value"));
    }

    #[test]
    fn test_properties_into_hashmap() {
        let props = Properties::new()
            .with_string("host", "localhost")
            .with_int("port", 5432);

        let map: HashMap<String, Value> = props.into();

        assert_eq!(map.len(), 2);
        assert_eq!(map.get("host").unwrap(), &json!("localhost"));
        assert_eq!(map.get("port").unwrap(), &json!(5432));
    }



    #[test]
    fn test_properties_complex_nested_value() {
        let complex = json!({
            "database": {
                "host": "localhost",
                "port": 5432,
                "credentials": {
                    "user": "admin",
                    "password": "secret"
                }
            },
            "options": ["opt1", "opt2"]
        });

        let props = Properties::new().with_value("config", complex.clone());

        let map = props.build();
        assert_eq!(map.get("config").unwrap(), &complex);
    }
}
