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


pub fn optional_query() -> &'static str {
  "
MATCH 
  (i:Invoice)
OPTIONAL MATCH
  (i)-[:RECONCILED_TO]->(p:Payment)
RETURN
  i.id AS id,
  i.amount AS amount,
  p.amount AS payment_amount
  "
}

pub fn optional_query_aggregating() -> &'static str {
    "
  MATCH 
    (i:Invoice)
  OPTIONAL MATCH
    (i)-[:RECONCILED_TO]->(p:Payment)
  RETURN
    i.id AS id,
    i.amount AS amount,
    sum(coalesce(p.amount, 0)) AS payment_amount,
    i.amount - sum(coalesce(p.amount, 0)) AS balance
    "
}

pub fn multi_optional_query() -> &'static str {
  "
MATCH
  (c:Customer)
OPTIONAL MATCH 
  (c)-[:HAS]->(i:Invoice)
OPTIONAL MATCH
  (i)-[:RECONCILED_TO]->(p:Payment)
RETURN
  c.id AS customer_id,
  i.id AS invoice_id,
  p.id AS payment_id
  "
}
