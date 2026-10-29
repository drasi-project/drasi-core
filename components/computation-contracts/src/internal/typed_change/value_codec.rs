// Copyright 2026 The Drasi Authors.
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

//! Adapters from the shared `drasi_core_types::data_value` codec to this crate's
//! `anyhow` boundary errors. The encoding itself lives only in core-types.

use std::collections::BTreeMap;

use drasi_core_types::{
    data_value::{self, DataValue},
    evaluation::variable_value::VariableValue,
};

pub(crate) use drasi_core_types::data_value::DataValue as Value;

pub(crate) fn encode_variables<K: AsRef<str>>(
    values: &BTreeMap<K, VariableValue>,
) -> anyhow::Result<BTreeMap<String, DataValue>> {
    Ok(data_value::encode_variables(values)?)
}

pub(crate) fn decode_variables<K: From<String> + Ord>(
    values: BTreeMap<String, DataValue>,
) -> anyhow::Result<BTreeMap<K, VariableValue>> {
    Ok(data_value::decode_variables(values)?)
}
