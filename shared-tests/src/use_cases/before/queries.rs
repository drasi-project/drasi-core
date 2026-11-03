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
WHERE i.amount > drasi.previousValue(i.amount)
RETURN
  i.id AS id,
  i.amount AS amount
  "
}

pub fn increasing_sum_query() -> &'static str {
    "
MATCH 
  (i:Invoice)-[:HAS]->(l:LineItem)
WITH
  i,
  sum(l.amount) AS total
WHERE total > drasi.previousValue(total, 0)
RETURN
  i.id AS id
  "
}

pub fn previous_value_types_query() -> &'static str {
    "
MATCH
  (n:Thing)
RETURN
  n.id AS id,
  drasi.previousValue(date(n.d)) AS date,
  drasi.previousValue(localtime(n.t)) AS local_time,
  drasi.previousValue(time(n.zt)) AS zoned_time,
  drasi.previousValue(duration(n.dur)) AS duration,
  drasi.previousValue([date(n.d)]) AS date_list,
  drasi.previousValue({d: date(n.d)}) AS date_map,
  drasi.previousValue(n) AS node,
  drasi.previousValue(date(n.d), date('2000-01-01')) AS date_with_default,
  drasi.previousValue({`__drasi_state_type__`: n.tag}) AS tagged_map,
  drasi.previousValue(n.lo..n.hi) AS range,
  drasi.previousValue([n.lo.., ..n.hi]) AS range_list
  "
}

pub fn previous_distinct_value_types_query() -> &'static str {
    "
MATCH
  (n:Thing)
RETURN
  n.id AS id,
  drasi.previousDistinctValue(date(n.d)) AS date,
  drasi.previousDistinctValue(localtime(n.t)) AS local_time,
  drasi.previousDistinctValue(time(n.zt)) AS zoned_time,
  drasi.previousDistinctValue(duration(n.dur)) AS duration,
  drasi.previousDistinctValue([date(n.d)]) AS date_list,
  drasi.previousDistinctValue({d: date(n.d)}) AS date_map,
  drasi.previousDistinctValue(n) AS node,
  drasi.previousDistinctValue(date(n.d), date('2000-01-01')) AS date_with_default,
  drasi.previousDistinctValue({`__drasi_state_type__`: n.tag}) AS tagged_map,
  drasi.previousDistinctValue(n.lo..n.hi) AS range,
  drasi.previousDistinctValue([n.lo.., ..n.hi]) AS range_list
  "
}

pub fn previous_distinct_date_query() -> &'static str {
    "
MATCH
  (n:Thing)
RETURN
  n.id AS id,
  drasi.previousDistinctValue(date(n.d)) AS date
  "
}
