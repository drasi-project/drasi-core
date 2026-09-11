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

use std::fmt;

/// The polling/processing clock, not an input's evaluation clock or the queue's next deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueCutoff(u64);

impl DueCutoff {
    pub fn new(observed_realtime: u64) -> Self {
        Self(observed_realtime)
    }

    pub fn includes(self, due_time: u64) -> bool {
        due_time <= self.0
    }

    /// Call again for every iteration of a drain, under the query's exclusive root.
    pub fn inspect(self, next_due_time: Option<u64>) -> DueHead {
        match next_due_time {
            None => DueHead::Empty,
            Some(due_time) if self.includes(due_time) => DueHead::Ready(ReadyHead { due_time }),
            Some(due_time) => DueHead::NotDue { due_time },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueHead {
    Empty,
    NotDue { due_time: u64 },
    Ready(ReadyHead),
}

/// Only `DueCutoff::inspect` can create permission to remove a due head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyHead {
    due_time: u64,
}

impl ReadyHead {
    /// The caller must mark the root dirty before dispatching pop. A mismatch aborts the root;
    /// it must never evaluate the unexpected ticket or turn the mismatch into an empty result.
    pub fn confirm_pop(self, popped_due_time: Option<u64>) -> Result<(), HeadChanged> {
        if popped_due_time != Some(self.due_time) {
            return Err(HeadChanged {
                expected: self.due_time,
                actual: popped_due_time,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadChanged {
    pub expected: u64,
    pub actual: Option<u64>,
}

impl fmt::Display for HeadChanged {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "due queue head changed under the query root: expected {}, found {:?}",
            self.expected, self.actual,
        )
    }
}

impl std::error::Error for HeadChanged {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn repeated_drain_stops_before_a_later_deadline() {
        let cutoff = DueCutoff::new(10);
        let mut queue =
            BTreeMap::from([((10, 0), "first"), ((10, 1), "second"), ((20, 2), "later")]);
        let mut delivered = Vec::new();
        loop {
            let head = queue.first_key_value().map(|((due, _), _)| *due);
            match cutoff.inspect(head) {
                DueHead::Ready(ready) => {
                    let popped = queue.pop_first();
                    ready
                        .confirm_pop(popped.as_ref().map(|((due, _), _)| *due))
                        .unwrap();
                    delivered.push(popped.unwrap().1);
                }
                DueHead::NotDue { due_time } => {
                    assert_eq!(due_time, 20);
                    break;
                }
                DueHead::Empty => panic!("drained a not-yet-due ticket"),
            }
        }
        assert_eq!(delivered, ["first", "second"]);
        assert_eq!(queue.len(), 1);
        assert_eq!(cutoff.inspect(Some(20)), DueHead::NotDue { due_time: 20 });
        assert!(matches!(
            DueCutoff::new(20).inspect(Some(20)),
            DueHead::Ready(_)
        ));
    }

    #[test]
    fn newly_scheduled_later_ticket_does_not_inherit_the_due_signal() {
        let cutoff = DueCutoff::new(15);
        let DueHead::Ready(ready) = cutoff.inspect(Some(10)) else {
            panic!("expected due ticket");
        };
        ready.confirm_pop(Some(10)).unwrap();
        assert_eq!(cutoff.inspect(Some(16)), DueHead::NotDue { due_time: 16 });
    }

    #[test]
    fn missing_or_changed_pop_is_an_error_not_an_empty_drain() {
        let DueHead::Ready(ready) = DueCutoff::new(10).inspect(Some(10)) else {
            panic!("expected due ticket");
        };
        assert_eq!(
            ready.confirm_pop(None),
            Err(HeadChanged {
                expected: 10,
                actual: None
            })
        );
        assert!(ready.confirm_pop(Some(9)).is_err());
        assert!(ready.confirm_pop(Some(11)).is_err());
        assert!(ready.confirm_pop(Some(10)).is_ok());
    }

    #[test]
    fn empty_and_timestamp_boundaries_do_not_overflow() {
        assert_eq!(DueCutoff::new(0).inspect(None), DueHead::Empty);
        assert!(matches!(
            DueCutoff::new(0).inspect(Some(0)),
            DueHead::Ready(_)
        ));
        assert_eq!(
            DueCutoff::new(0).inspect(Some(u64::MAX)),
            DueHead::NotDue { due_time: u64::MAX }
        );
        assert!(matches!(
            DueCutoff::new(u64::MAX).inspect(Some(u64::MAX)),
            DueHead::Ready(_)
        ));
    }
}
