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

extern crate alloc;
use super::VariableValue;
use alloc::string::String;
use core::fmt::{self, Display};
use std::collections::BTreeMap;

// A type that can be used to index into a `VariableValue`.
pub trait Index: private::Sealed {
    #[doc(hidden)]
    fn index_into<'a>(&self, value: &'a VariableValue) -> Option<&'a VariableValue>;

    #[doc(hidden)]
    fn index_into_mut<'a>(&self, value: &'a mut VariableValue) -> Option<&'a mut VariableValue>;

    #[doc(hidden)]
    fn index_or_insert<'a>(&self, value: &'a mut VariableValue) -> &'a mut VariableValue;
}

impl Index for usize {
    fn index_into<'a>(&self, value: &'a VariableValue) -> Option<&'a VariableValue> {
        match value {
            VariableValue::List(list) => list.get(*self),
            _ => None,
        }
    }

    fn index_into_mut<'a>(&self, value: &'a mut VariableValue) -> Option<&'a mut VariableValue> {
        match value {
            VariableValue::List(list) => list.get_mut(*self),
            _ => None,
        }
    }

    fn index_or_insert<'a>(&self, value: &'a mut VariableValue) -> &'a mut VariableValue {
        match value {
            VariableValue::List(list) => {
                let len = list.len();
                list.get_mut(*self).unwrap_or_else(|| {
                    panic!("cannot access index {self} of JSON array of length {len}")
                })
            }
            _ => panic!("cannot access index {} of JSON {}", self, Type(value)),
        }
    }
}

impl Index for str {
    fn index_into<'a>(&self, value: &'a VariableValue) -> Option<&'a VariableValue> {
        match value {
            VariableValue::Object(map) => map.get(self),
            _ => None,
        }
    }

    fn index_into_mut<'a>(&self, value: &'a mut VariableValue) -> Option<&'a mut VariableValue> {
        match value {
            VariableValue::Object(map) => map.get_mut(self),
            _ => None,
        }
    }

    fn index_or_insert<'a>(&self, value: &'a mut VariableValue) -> &'a mut VariableValue {
        if let VariableValue::Null = value {
            *value = VariableValue::Object(BTreeMap::new());
        }
        match value {
            VariableValue::Object(map) => map.entry(self.to_owned()).or_insert(VariableValue::Null),
            _ => panic!("cannot access index {} of JSON {}", self, Type(value)),
        }
    }
}

impl Index for String {
    fn index_into<'a>(&self, value: &'a VariableValue) -> Option<&'a VariableValue> {
        //borrowing the entire collection
        self[..].index_into(value)
    }

    fn index_into_mut<'a>(&self, value: &'a mut VariableValue) -> Option<&'a mut VariableValue> {
        self[..].index_into_mut(value)
    }

    fn index_or_insert<'a>(&self, value: &'a mut VariableValue) -> &'a mut VariableValue {
        self[..].index_or_insert(value)
    }
}

impl<T> Index for &T
where
    T: ?Sized + Index,
{
    fn index_into<'v>(&self, v: &'v VariableValue) -> Option<&'v VariableValue> {
        (**self).index_into(v)
    }
    fn index_into_mut<'v>(&self, v: &'v mut VariableValue) -> Option<&'v mut VariableValue> {
        (**self).index_into_mut(v)
    }

    fn index_or_insert<'v>(&self, v: &'v mut VariableValue) -> &'v mut VariableValue {
        (**self).index_or_insert(v)
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for usize {}
    impl Sealed for str {}
    impl Sealed for String {}
    impl<T> Sealed for &T where T: ?Sized + Sealed {}
}

struct Type<'a>(&'a VariableValue);

impl Display for Type<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.0 {
            VariableValue::Null => f.write_str("null"),
            VariableValue::Bool(_) => f.write_str("boolean"),
            VariableValue::Float(_) => f.write_str("float"),
            VariableValue::Integer(_) => f.write_str("integer"),
            VariableValue::String(_) => f.write_str("string"),
            VariableValue::List(_) => f.write_str("list"),
            VariableValue::Object(_) => f.write_str("object"),
            VariableValue::Date(_) => f.write_str("date"),
            VariableValue::LocalTime(_) => f.write_str("localtime"),
            VariableValue::ZonedTime(_) => f.write_str("zonedtime"),
            VariableValue::LocalDateTime(_) => f.write_str("localdatetime"),
            VariableValue::ZonedDateTime(_) => f.write_str("zoneddatetime"),
            VariableValue::Duration(_) => f.write_str("duration"),
            VariableValue::Expression(_) => f.write_str("expression"),
            VariableValue::ListRange(_) => f.write_str("listrange"),
            VariableValue::Element(_) => f.write_str("element"),
            VariableValue::ElementMetadata(_) => f.write_str("elementmetadata"),
            VariableValue::ElementReference(_) => f.write_str("elementreference"),
            VariableValue::Awaiting => f.write_str("awaiting"),
        }
    }
}
