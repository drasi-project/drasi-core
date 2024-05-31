use std::{
    collections::{btree_map::Entry, BTreeMap},
    hash::{Hash, Hasher},
    sync::Arc,
};

use hashers::fx_hash::FxHasher;
use ordered_float::OrderedFloat;

use crate::interface::IndexError;
use crate::{evaluation::variable_value::VariableValue, interface::ResultIndex};

#[derive(Clone)]
pub struct SortedSetEntryCount {
    pub snapshot: isize,
    pub delta: isize,
}

pub type SortedSetChangeLog = BTreeMap<OrderedFloat<f64>, SortedSetEntryCount>;

#[derive(Clone)]
pub struct LazySortedSet {
    set_id: u64,
    change_log: SortedSetChangeLog,
    store: Arc<dyn ResultIndex>, //todo: change to LazySortedSetStore after trait upcasting is supported by stable rust
}

impl LazySortedSet {
    pub fn new(
        position_in_query: usize,
        grouping_values: &Vec<VariableValue>,
        store: Arc<dyn ResultIndex>,
    ) -> LazySortedSet {
        let mut hasher = FxHasher::default();
        position_in_query.hash(&mut hasher);
        for value in grouping_values {
            value.to_string().hash(&mut hasher);
        }

        LazySortedSet {
            set_id: hasher.finish(),
            change_log: SortedSetChangeLog::new(),
            store,
        }
    }

    pub async fn get_head(&self) -> Result<Option<f64>, IndexError> {
        let mut storage_cursor = self.store.get_next(self.set_id, None).await?;

        for (val, entry) in &self.change_log {
            if let Some(sc) = storage_cursor {
                if val > &sc.0 {
                    return Ok(Some(sc.0.into()));
                }
                storage_cursor = self.store.get_next(self.set_id, Some(sc.0)).await?;
            }

            if entry.snapshot + entry.delta > 0 {
                return Ok(Some(val.0));
            }
        }

        match storage_cursor {
            Some(sc) => Ok(Some(sc.0.into())),
            None => Ok(None),
        }
    }

    pub async fn insert(&mut self, value: f64) {
        match self.change_log.entry(value.into()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().delta += 1;
            }
            Entry::Vacant(entry) => {
                let stored_count = self
                    .store
                    .get_value_count(self.set_id, value.into())
                    .await
                    .unwrap_or(0);
                entry.insert(SortedSetEntryCount {
                    snapshot: stored_count,
                    delta: 1,
                });
            }
        };
    }

    pub async fn remove(&mut self, value: f64) {
        match self.change_log.entry(value.into()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().delta -= 1;
            }
            Entry::Vacant(entry) => {
                let stored_count = self
                    .store
                    .get_value_count(self.set_id, value.into())
                    .await
                    .unwrap_or(0);
                entry.insert(SortedSetEntryCount {
                    snapshot: stored_count,
                    delta: -1,
                });
            }
        };
    }

    pub async fn commit(&mut self) -> Result<(), IndexError> {
        for (value, entry) in &self.change_log {
            self.store
                .increment_value_count(self.set_id, value.clone(), entry.delta)
                .await?;
        }
        self.change_log.clear();
        Ok(())
    }
}
