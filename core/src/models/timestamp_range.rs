use crate::models::ElementTimestamp;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct TimestampRange<T> {
    pub from: TimestampBound<T>,
    pub to: ElementTimestamp,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum TimestampBound<T> {
    StartFromPrevious(T),
    Included(T),
}

impl<T> TimestampBound<T> {
    // &TimestampBound<T> -> TimestampBound<&T>
    pub fn as_ref(&self) -> TimestampBound<&T> {
        match *self {
            TimestampBound::StartFromPrevious(ref t) => TimestampBound::StartFromPrevious(t),
            TimestampBound::Included(ref t) => TimestampBound::Included(t),
        }
    }

    // &mut TimestampBound<T> -> TimestampBound<&mut T>
    pub fn as_mut(&mut self) -> TimestampBound<&mut T> {
        match *self {
            TimestampBound::StartFromPrevious(ref mut t) => TimestampBound::StartFromPrevious(t),
            TimestampBound::Included(ref mut t) => TimestampBound::Included(t),
        }
    }

    pub fn get_timestamp(&self) -> &T {
        match self {
            TimestampBound::StartFromPrevious(t) => t,
            TimestampBound::Included(t) => t,
        }
    }
}

impl<T: Clone> TimestampBound<&T> {
    pub fn cloned(self) -> TimestampBound<T> {
        match self {
            TimestampBound::StartFromPrevious(t) => TimestampBound::StartFromPrevious(t.clone()),
            TimestampBound::Included(t) => TimestampBound::Included(t.clone()),
        }
    }
}
