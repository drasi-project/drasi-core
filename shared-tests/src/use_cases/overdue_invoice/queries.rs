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

pub fn list_overdue_query() -> &'static str {
    "
    MATCH 
        (a:Invoice)
    WHERE drasi.trueUntil(
        a.status = 'unpaid', 
        date(a.invoiceDate) + duration({days: 3})
    )
    RETURN
        a.invoiceNumber as invoiceNumber,
        a.invoiceDate as invoiceDate
    "
}

pub fn count_overdue_greater_query() -> &'static str {
    "
    MATCH 
        (a:Invoice)
    WHERE a.status = 'unpaid'
    WITH
        count(a) as count
    WHERE drasi.trueUntil(count > 2, date.transaction() + duration({days: 3}))
    RETURN
        count
    "
}
