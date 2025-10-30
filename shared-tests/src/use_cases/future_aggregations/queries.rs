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

pub fn truefor_sum_query() -> &'static str {
    "
    MATCH (o:Order)
    WHERE o.status = 'ready'
    AND drasi.trueFor(sum(o.value) > 1, duration ({ seconds: 5 }))
    RETURN
      sum(o.value) as value
    "
}

pub fn truefor_sum_grouping_query() -> &'static str {
    "
    MATCH (o:Order)
    WHERE o.status = 'ready'
    AND drasi.trueFor(sum(o.value) > 1, duration ({ seconds: 5 }))
    RETURN
      sum(o.value) as value,
      o.category as category
    "
}

pub fn truelater_max_query() -> &'static str {
    "
    MATCH
      (i:Issue)
    OPTIONAL MATCH
      (i)-[:HAS]->(c:Comment)
    WHERE i.state = 'open'
   
    WHERE drasi.trueLater(
        (max(datetime(coalesce(c.created_at, i.created_at))) + duration({ seconds: 10 })) <= datetime.realtime(),
        datetime.transaction() + duration({ seconds: 10 })
      )
    OR (max(datetime(coalesce(c.created_at, i.created_at))) + duration({ seconds: 10 })) <= datetime.realtime()
   
    RETURN
      i.id AS id
    "
}
