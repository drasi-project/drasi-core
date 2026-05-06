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

pub fn starts_with_query() -> &'static str {
    "MATCH (p:Product) WHERE p.name STARTS WITH 'Drasi' RETURN p.name AS name"
}

pub fn ends_with_query() -> &'static str {
    "MATCH (p:Product) WHERE p.name ENDS WITH 'Tool' RETURN p.name AS name"
}

pub fn contains_query() -> &'static str {
    "MATCH (p:Product) WHERE p.name CONTAINS 'Query' RETURN p.name AS name"
}
