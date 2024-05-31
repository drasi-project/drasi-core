#![allow(dead_code)]

extern crate generational_arena;
use drasi_core::models::ElementPropertyMap;
use generational_arena::{Arena, Index};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drasi_core::{
    evaluation::variable_value::VariableValue,
    models::{Element, ElementMetadata, ElementReference},
};

#[derive(Debug)]
pub enum GraphChangeResult<T> {
    Insert { element: T },
    Update { element_before: T, element_after: T },
    Delete { element: T },
}

#[derive(Debug, Clone)]
pub enum GraphError {
    InvalidNodeId { id: NodeId },
    DuplicateNodeId { id: NodeId },
    InvalidRelationId { id: RelationId },
    DuplicateRelationId { id: RelationId },
}

// GraphFilter is an enum that is used to express filters that are applied to Nodes and Relations when
// querying a DomainModelGraph for sets of Nodes and Relations. Options are:
// - All: Return all Nodes or Relations
// - ById: Return Nodes or Relations with the specified Ids
// - ByLabel: Return Nodes or Relations with the specified Labels
// - None: Return no Nodes or Relations (an empty Vec)
#[derive(Debug)]
pub enum GraphFilter {
    All,
    ById(Vec<String>),
    ByLabel(Vec<String>),
    None,
}

pub struct GraphIds {
    pub node_ids: Vec<NodeId>,
    pub relation_ids: Vec<RelationId>,
}

impl GraphIds {
    pub fn new(node_ids: Vec<NodeId>, relation_ids: Vec<RelationId>) -> GraphIds {
        GraphIds {
            node_ids: node_ids,
            relation_ids: relation_ids,
        }
    }
}

// Define a type alias for a NodeId
pub type NodeId = String;

// Define a Node struct
#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub effective_from: u64,
    pub source_id: String,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, VariableValue>,
    pub relation_ids: Vec<RelationId>,
}

impl Node {
    pub fn new(
        id: NodeId,
        effective_from: u64,
        source_id: String,
        labels: Vec<String>,
        properties: BTreeMap<String, VariableValue>,
    ) -> Node {
        Node {
            id: id,
            effective_from: effective_from,
            source_id: source_id,
            labels: labels,
            properties: properties,
            relation_ids: Vec::new(),
        }
    }

    pub fn into_element(self) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference {
                    element_id: self.id.into(),
                    source_id: self.source_id.into(),
                },
                labels: Arc::from(
                    self.labels
                        .into_iter()
                        .map(Arc::from)
                        .collect::<Vec<Arc<str>>>(),
                ),
                effective_from: self.effective_from,
            },
            properties: (&self.properties).into(),
        }
    }
}

// Define a type alias for a RelationId
pub type RelationId = String;

// Define a Relation struct
#[derive(Debug, Clone)]
pub struct Relation {
    pub id: RelationId,
    pub effective_from: u64,
    pub source_id: String,
    pub labels: Vec<String>,
    pub from_id: NodeId,
    pub to_id: NodeId,
}

impl Relation {
    pub fn new(
        id: RelationId,
        effective_from: u64,
        source_id: String,
        labels: Vec<String>,
        from_id: String,
        to_id: String,
    ) -> Relation {
        Relation {
            id: id,
            effective_from: effective_from,
            source_id: source_id,
            labels: labels,
            from_id: from_id,
            to_id: to_id,
        }
    }

    pub fn into_element(self) -> Element {
        Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference {
                    element_id: self.id.into(),
                    source_id: Arc::from(self.source_id.as_str()),
                },
                labels: Arc::from(
                    self.labels
                        .into_iter()
                        .map(Arc::from)
                        .collect::<Vec<Arc<str>>>(),
                ),
                effective_from: self.effective_from,
            },
            in_node: ElementReference {
                element_id: self.from_id.into(),
                source_id: Arc::from(self.source_id.as_str()),
            },
            out_node: ElementReference {
                element_id: self.to_id.into(),
                source_id: Arc::from(self.source_id.as_str()),
            },
            properties: ElementPropertyMap::new(),
        }
    }
}

// Define a DomainModelGraph struct.
// The DomainModelGraph struct contains two Arenas, one for Nodes and one for Relations.
// The Node Arena is used to store Node structs and the Relation Arena is used to store Relation structs.
// The DomainModelGraph struct also contains a HashMap that maps NodeIds to NodeIndexs and a HashMap that maps RelationIds to RelationIndexs.
pub struct DomainModelGraph {
    nodes: Arena<Node>,
    relations: Arena<Relation>,
    node_index_map: HashMap<NodeId, Index>,
    relation_index_map: HashMap<RelationId, Index>,
}

// Implement the Graph struct new method
impl DomainModelGraph {
    pub fn new() -> DomainModelGraph {
        DomainModelGraph {
            nodes: Arena::new(),
            relations: Arena::new(),
            node_index_map: HashMap::new(),
            relation_index_map: HashMap::new(),
        }
    }
}

impl DomainModelGraph {
    pub fn add_node(&mut self, node: Node) -> Result<GraphChangeResult<Node>, GraphError> {
        let node_id = node.id.clone();

        // Ensure the NodeId is not already in use
        if self.node_index_map.contains_key(&node_id) {
            return Err(GraphError::DuplicateNodeId {
                id: node_id.clone(),
            });
        }

        let node_index = self.nodes.insert(node);
        self.node_index_map.insert(node_id.clone(), node_index);
        Ok(GraphChangeResult::Insert {
            element: self.nodes[node_index].clone(),
        })
    }

    pub fn add_relation(
        &mut self,
        relation: Relation,
    ) -> Result<GraphChangeResult<Relation>, GraphError> {
        let relation_id = relation.id.clone();

        // Ensure the RelationId is not already in use
        if self.relation_index_map.contains_key(&relation_id) {
            return Err(GraphError::DuplicateRelationId {
                id: relation_id.clone(),
            });
        }

        // Ensure the from_id and to_id are valid NodeIds
        let from_node_index =
            self.node_index_map
                .get(&relation.from_id)
                .ok_or(Err::<Relation, GraphError>(GraphError::InvalidNodeId {
                    id: relation.from_id.clone(),
                }));

        let to_node_index =
            self.node_index_map
                .get(&relation.to_id)
                .ok_or(Err::<Relation, GraphError>(GraphError::InvalidNodeId {
                    id: relation.to_id.clone(),
                }));

        let relation_index = self.relations.insert(relation);
        self.relation_index_map
            .insert(relation_id.clone(), relation_index);

        // Add the relation_id to the Node referenced by the from_node_index
        let from_node_index = from_node_index.unwrap();
        let from_node = self.nodes.get_mut(*from_node_index).unwrap();
        from_node.relation_ids.push(relation_id.clone());

        // Add the relation_id to the Node referenced by the to_node_index
        let to_node_index = to_node_index.unwrap();
        let to_node = self.nodes.get_mut(*to_node_index).unwrap();
        to_node.relation_ids.push(relation_id.clone());

        Ok(GraphChangeResult::Insert {
            element: self.relations[relation_index].clone(),
        })
    }

    pub fn upsert_node(&mut self, node: Node) -> Result<GraphChangeResult<Node>, GraphError> {
        if let Some(node_index) = self.node_index_map.get(&node.id) {
            let node_before = self.nodes[*node_index].clone();
            self.nodes[*node_index] = node.clone();
            Ok(GraphChangeResult::Update {
                element_before: node_before,
                element_after: node,
            })
        } else {
            self.add_node(node)
        }
    }

    pub fn get_node(&self, id: NodeId) -> Result<Node, GraphError> {
        if let Some(node_index) = self.node_index_map.get(&id) {
            Ok(self.nodes[*node_index].clone())
        } else {
            Err(GraphError::InvalidNodeId { id: id.clone() })
        }
    }

    pub fn get_relation(&self, id: RelationId) -> Result<Relation, GraphError> {
        if let Some(relation_index) = self.relation_index_map.get(&id) {
            Ok(self.relations[*relation_index].clone())
        } else {
            Err(GraphError::InvalidRelationId { id: id.clone() })
        }
    }

    pub fn delete_node(&mut self, id: NodeId) -> Result<GraphChangeResult<Node>, GraphError> {
        // Try remove the Node Id from the node_index_map and get the Node Index
        if let Some(node_index) = self.node_index_map.remove(&id) {
            // Return the removed Node
            Ok(GraphChangeResult::Delete {
                element: self.nodes.remove(node_index).unwrap(),
            })
        } else {
            Err(GraphError::InvalidNodeId { id: id.clone() })
        }
    }

    pub fn delete_relation(
        &mut self,
        id: RelationId,
    ) -> Result<GraphChangeResult<Relation>, GraphError> {
        // Try remove the Relation Id from the relation_index_map and get the Relation Index
        if let Some(relation_index) = self.relation_index_map.remove(&id) {
            // Return the removed Relation
            Ok(GraphChangeResult::Delete {
                element: self.relations.remove(relation_index).unwrap(),
            })
        } else {
            Err(GraphError::InvalidRelationId { id: id.clone() })
        }
    }

    // Returns a GraphIds struct containing all the NodeIds and RelationIds that match the specified GraphFilter.
    // Takes two arguments, one GraphFilter for Nodes and one GraphFilter for Relations.
    pub fn get_graph_ids_by_filter(
        &self,
        node_filter: GraphFilter,
        relation_filter: GraphFilter,
    ) -> GraphIds {
        let node_ids = match node_filter {
            GraphFilter::All => self.node_index_map.keys().cloned().collect::<Vec<NodeId>>(),
            GraphFilter::ById(ids) => {
                let mut node_ids = Vec::new();
                for id in ids {
                    if self.node_index_map.contains_key(&id) {
                        node_ids.push(id.clone());
                    }
                }
                node_ids
            }
            GraphFilter::ByLabel(labels) => {
                let mut node_ids = Vec::new();
                for (_, node) in self.nodes.iter() {
                    for label in &labels {
                        if node.labels.contains(label) {
                            node_ids.push(node.id.clone());
                        }
                    }
                }
                node_ids
            }
            GraphFilter::None => Vec::new(),
        };

        let relation_ids = match relation_filter {
            GraphFilter::All => self
                .relation_index_map
                .keys()
                .cloned()
                .collect::<Vec<RelationId>>(),
            GraphFilter::ById(ids) => {
                let mut relation_ids = Vec::new();
                for id in ids {
                    if self.relation_index_map.contains_key(&id) {
                        relation_ids.push(id.clone());
                    }
                }
                relation_ids
            }
            GraphFilter::ByLabel(labels) => {
                let mut relation_ids = Vec::new();
                for (_, relation) in self.relations.iter() {
                    for label in &labels {
                        if relation.labels.contains(label) {
                            relation_ids.push(relation.id.clone());
                        }
                    }
                }
                relation_ids
            }
            GraphFilter::None => Vec::new(),
        };

        GraphIds::new(node_ids, relation_ids)
    }
}
