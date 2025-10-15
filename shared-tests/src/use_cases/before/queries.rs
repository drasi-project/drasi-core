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

pub fn increasing_value_query() -> &'static str {
    "
MATCH 
  (i:Invoice)
WHERE i.amount > drasi.before(i.amount)
RETURN
  i.id AS id,
  i.amount AS amount
  "
}

pub fn increasing_sum_query() -> &'static str {
    "
MATCH 
  (i:Invoice)-[:HAS]->(l:LineItem)
WHERE sum(l.amount) > drasi.before(sum(l.amount), 0)
RETURN
  i.id AS id
  "
}

