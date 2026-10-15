// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Retired query utilities retained for existing Rust callers.
//!
//! ComputationGraph does not use these helpers to execute queries or retain
//! query output. New applications should use graph-owned query construction and
//! result access instead. The original `queries` import paths remain available.

mod query_base;
mod sequence_dedup;

#[allow(deprecated)]
pub use crate::queries::output_state::QueryOutputState;
#[allow(deprecated)]
pub use query_base::QueryBase;
#[allow(deprecated)]
pub use sequence_dedup::SequenceDedup;

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use std::any::TypeId;

    #[test]
    fn original_query_helper_imports_refer_to_the_same_types() {
        assert_eq!(
            TypeId::of::<QueryBase>(),
            TypeId::of::<crate::queries::base::QueryBase>()
        );
        assert_eq!(
            TypeId::of::<QueryBase>(),
            TypeId::of::<crate::queries::QueryBase>()
        );
        assert_eq!(
            TypeId::of::<SequenceDedup>(),
            TypeId::of::<crate::queries::sequence_dedup::SequenceDedup>()
        );
        assert_eq!(
            TypeId::of::<SequenceDedup>(),
            TypeId::of::<crate::queries::SequenceDedup>()
        );
        assert_eq!(
            TypeId::of::<QueryOutputState>(),
            TypeId::of::<crate::queries::output_state::QueryOutputState>()
        );
        assert_eq!(
            TypeId::of::<QueryOutputState>(),
            TypeId::of::<crate::queries::QueryOutputState>()
        );
    }
}
