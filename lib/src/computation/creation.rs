// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::v1::{ComponentId, GraphError, GraphResult};

/// UUID allocation scoped to one DrasiLib-owned batch constructor.
/// Allocations are not reusable admission permits and cannot be serialized.
pub struct ComponentIds {
    issued: std::collections::BTreeSet<ComponentId>,
    allocations: Vec<crate::management::ComponentIdAllocation>,
    replay: Option<Vec<crate::management::ComponentIdAllocation>>,
    allow_external: bool,
}

impl ComponentIds {
    pub(crate) fn for_request(
        allow_external: bool,
        replay: Option<Vec<crate::management::ComponentIdAllocation>>,
    ) -> Self {
        Self {
            issued: Default::default(),
            allocations: Vec::new(),
            replay,
            allow_external,
        }
    }

    /// Allocate the identity of one new component in this construction request.
    pub fn allocate(&mut self) -> GraphResult<ComponentId> {
        self.allocate_requested(None)
    }

    /// Allocate an omitted ID or accept the exact requested UUID when this
    /// instance permits external IDs. This setting cannot be supplied by callers.
    pub fn allocate_requested(
        &mut self,
        requested: Option<ComponentId>,
    ) -> GraphResult<ComponentId> {
        if let Some(id) = &requested {
            if !self.allow_external {
                return Err(GraphError::ExternalComponentId {
                    component: id.clone(),
                });
            }
        }
        let id = match &self.replay {
            Some(replay) => {
                let allocation =
                    replay
                        .get(self.allocations.len())
                        .ok_or_else(|| GraphError::Topology {
                            reason: "creation retry changed its allocation count".into(),
                        })?;
                if allocation.requested != requested {
                    return Err(GraphError::Topology {
                        reason: "creation retry changed a requested identity".into(),
                    });
                }
                allocation.assigned.clone()
            }
            None => requested.clone().unwrap_or_else(ComponentId::generate),
        };
        self.allocations
            .push(crate::management::ComponentIdAllocation {
                requested,
                assigned: id.clone(),
            });
        self.issued.insert(id.clone());
        Ok(id)
    }

    pub(crate) fn finish(&self) -> GraphResult<Vec<crate::management::ComponentIdAllocation>> {
        if self
            .replay
            .as_ref()
            .is_some_and(|prior| prior.len() != self.allocations.len())
        {
            return Err(GraphError::Topology {
                reason: "creation retry omitted an allocation".into(),
            });
        }
        Ok(self.allocations.clone())
    }

    /// Derive a framework helper from an owner allocated in this same request.
    /// The key must be immutable, never an editable component name. Possession
    /// of an old UUID, including a removed owner, grants no authority here.
    pub fn derive_owned(&mut self, owner: &ComponentId, key: &str) -> GraphResult<ComponentId> {
        self.require_owned(owner)?;
        self.authorize_derived(owner, ComponentId::derive_owned(owner, key)?)
    }

    pub(crate) fn require_owned(&self, owner: &ComponentId) -> GraphResult<()> {
        if !self.issued.contains(owner) {
            return Err(GraphError::ExternalComponentId {
                component: owner.clone(),
            });
        }
        Ok(())
    }

    pub(crate) fn authorize_derived(
        &mut self,
        owner: &ComponentId,
        helper: ComponentId,
    ) -> GraphResult<ComponentId> {
        self.require_owned(owner)?;
        self.issued.insert(helper.clone());
        Ok(helper)
    }
}
