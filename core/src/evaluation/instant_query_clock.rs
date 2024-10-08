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

use crate::{interface::QueryClock, models::SourceChange};

#[derive(Debug)]
pub struct InstantQueryClock {
    transaction_time: u64,
    realtime: u64,
}

impl InstantQueryClock {
    pub fn new(transaction_time: u64, realtime: u64) -> InstantQueryClock {
        InstantQueryClock {
            transaction_time,
            realtime,
        }
    }

    pub fn from_source_change(change: &SourceChange) -> InstantQueryClock {
        InstantQueryClock {
            transaction_time: change.get_transaction_time(),
            realtime: change.get_realtime(),
        }
    }
}

impl QueryClock for InstantQueryClock {
    fn get_transaction_time(&self) -> u64 {
        self.transaction_time
    }

    fn get_realtime(&self) -> u64 {
        self.realtime
    }
}
