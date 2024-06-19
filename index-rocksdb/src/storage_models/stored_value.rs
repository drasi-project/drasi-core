use std::{collections::HashMap, hash::Hasher, sync::Arc};

use drasi_core::models::{ElementPropertyMap, ElementValue};

#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredValueContainer {
    #[prost(oneof = "StoredValue", tags = "1, 2, 3, 4, 5, 6, 7")]
    pub value: ::core::option::Option<StoredValue>,
}

#[derive(PartialEq, Clone, ::prost::Oneof)]
pub enum StoredValue {
    #[prost(bool, tag = "1")]
    Bool(bool),

    #[prost(double, tag = "2")]
    Float(f64),

    #[prost(int64, tag = "3")]
    Integer(i64),

    #[prost(string, tag = "4")]
    String(String),

    #[prost(message, tag = "5")]
    List(StoredValueList),

    #[prost(message, tag = "6")]
    Object(StoredValueMap),
}

impl std::hash::Hash for StoredValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            StoredValue::Bool(b) => {
                0.hash(state);
                b.hash(state)
            }
            StoredValue::Float(f) => {
                1.hash(state);
                f.to_bits().hash(state)
            }
            StoredValue::Integer(i) => {
                2.hash(state);
                i.hash(state)
            }
            StoredValue::String(s) => {
                3.hash(state);
                s.hash(state)
            }
            StoredValue::List(l) => {
                4.hash(state);
                l.hash(state)
            }
            StoredValue::Object(o) => {
                5.hash(state);
                o.hash(state)
            }
        }
    }
}

#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredValueList {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<StoredValueContainer>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StoredValueMap {
    #[prost(map = "string, message", tag = "1")]
    pub values: HashMap<String, StoredValueContainer>,
}

impl StoredValueMap {
    pub fn new() -> Self {
        StoredValueMap {
            values: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&StoredValue> {
        match self.values.get(key) {
            Some(v) => v.value.as_ref(),
            None => None,
        }
    }
}

impl std::hash::Hash for StoredValueMap {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for (key, value) in self.values.iter() {
            key.hash(state);
            value.hash(state);
        }
    }
}

impl From<&ElementPropertyMap> for StoredValueMap {
    fn from(map: &ElementPropertyMap) -> Self {
        let mut values = HashMap::new();

        map.map_iter(|key, value| (key.to_string(), value.into()))
            .for_each(|(key, value)| {
                values.insert(key, value);
            });

        StoredValueMap { values }
    }
}

impl From<StoredValueMap> for ElementPropertyMap {
    fn from(val: StoredValueMap) -> Self {
        let mut map = ElementPropertyMap::new();

        for (key, value) in val.values {
            map.insert(&key, value.into());
        }

        map
    }
}

impl From<&ElementValue> for StoredValueContainer {
    fn from(value: &ElementValue) -> Self {
        match value {
            ElementValue::Null => StoredValueContainer { value: None },
            ElementValue::Bool(b) => StoredValueContainer {
                value: Some(StoredValue::Bool(*b)),
            },
            ElementValue::Float(f) => StoredValueContainer {
                value: Some(StoredValue::Float(f.into_inner())),
            },
            ElementValue::Integer(i) => StoredValueContainer {
                value: Some(StoredValue::Integer(*i)),
            },
            ElementValue::String(s) => StoredValueContainer {
                value: Some(StoredValue::String(s.to_string())),
            },
            ElementValue::List(l) => StoredValueContainer {
                value: Some(StoredValue::List(StoredValueList {
                    values: l.iter().map(|v| v.into()).collect(),
                })),
            },
            ElementValue::Object(o) => StoredValueContainer {
                value: Some(StoredValue::Object(o.into())),
            },
        }
    }
}

impl From<StoredValueContainer> for ElementValue {
    fn from(val: StoredValueContainer) -> Self {
        match val.value {
            None => ElementValue::Null,
            Some(StoredValue::Bool(b)) => ElementValue::Bool(b),
            Some(StoredValue::Float(f)) => ElementValue::Float(f.into()),
            Some(StoredValue::Integer(i)) => ElementValue::Integer(i),
            Some(StoredValue::String(s)) => ElementValue::String(Arc::from(s.as_str())),
            Some(StoredValue::List(l)) => {
                ElementValue::List(l.values.into_iter().map(|v| v.into()).collect())
            }
            Some(StoredValue::Object(o)) => ElementValue::Object(o.into()),
        }
    }
}
