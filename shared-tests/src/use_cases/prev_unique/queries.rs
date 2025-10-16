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

pub fn prev_unique_query() -> &'static str {
    "
MATCH 
  (c:Contract)
WHERE c.status = 'active'
AND drasi.previousUniqueValue(c.status) = 'pending'
RETURN
  c.id AS id,
  c.status AS status,
  c.tags AS tags
  "
}
