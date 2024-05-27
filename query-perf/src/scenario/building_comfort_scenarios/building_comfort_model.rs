use std::{
    collections::BTreeMap,
    fmt,
    hash::{Hash, Hasher},
    sync::{Arc, RwLock},
};

use drasi_query_core::{
    evaluation::variable_value::{float::Float, VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};

use crate::{
    models::{
        domain_model_graph::{
            DomainModelGraph, GraphChangeResult, GraphFilter, GraphIds, Node, NodeId, Relation,
        },
        model_change::ModelChangeResult,
    },
    scenario::SourceChangeGenerator,
};

#[derive(Debug, Clone, Copy, Eq)]
pub enum Location {
    Building(usize),
    Floor(usize, usize),
    Room(usize, usize, usize),
}

impl Location {
    // pub fn new_building(building_idx: usize) -> Location {
    //     Location::Building(building_idx)
    // }

    // pub fn new_floor(building_idx: usize, floor_idx: usize) -> Location {
    //     Location::Floor(building_idx, floor_idx)
    // }

    pub fn new_floor_from_building_location(
        building_location: Location,
        floor_idx: usize,
    ) -> Location {
        match building_location {
            Location::Building(building_idx) => Location::Floor(building_idx, floor_idx),
            _ => panic!("Invalid Location type."),
        }
    }

    pub fn new_room(building_idx: usize, floor_idx: usize, room_idx: usize) -> Location {
        Location::Room(building_idx, floor_idx, room_idx)
    }

    pub fn new_room_from_floor_location(floor_location: Location, room_idx: usize) -> Location {
        match floor_location {
            Location::Floor(building_idx, floor_idx) => {
                Location::Room(building_idx, floor_idx, room_idx)
            }
            _ => panic!("Invalid Location type."),
        }
    }

    pub fn from_string(s: &str) -> Location {
        let parts: Vec<&str> = s.split('_').collect();
        match parts[0] {
            "b" => Location::Building(parts[1].parse::<usize>().unwrap()),
            "f" => Location::Floor(
                parts[1].parse::<usize>().unwrap(),
                parts[2].parse::<usize>().unwrap(),
            ),
            "r" => Location::Room(
                parts[1].parse::<usize>().unwrap(),
                parts[2].parse::<usize>().unwrap(),
                parts[3].parse::<usize>().unwrap(),
            ),
            &_ => panic!("Invalid Location string."),
        }
    }

    // pub fn to_string(&self) -> String {
    //     match self {
    //         Location::Building(building_idx) => format!("b_{}", building_idx),
    //         Location::Floor(building_idx, floor_idx) => format!("f_{}_{}", building_idx, floor_idx),
    //         Location::Room(building_idx, floor_idx, room_idx) => {
    //             format!("r_{}_{}_{}", building_idx, floor_idx, room_idx)
    //         }
    //     }
    // }

    pub fn get_building_idx(&self) -> usize {
        match self {
            Location::Building(building_idx) => *building_idx,
            Location::Floor(building_idx, _) => *building_idx,
            Location::Room(building_idx, _, _) => *building_idx,
        }
    }

    // pub fn get_floor_idx(&self) -> usize {
    //     match self {
    //         Location::Floor(_, floor_idx) => *floor_idx,
    //         Location::Room(_, floor_idx, _) => *floor_idx,
    //         _ => panic!("Invalid Location type."),
    //     }
    // }

    // pub fn get_room_idx(&self) -> usize {
    //     match self {
    //         Location::Room(_, _, room_idx) => *room_idx,
    //         _ => panic!("Invalid Location type."),
    //     }
    // }
}

impl Hash for Location {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Location::Building(building_idx) => {
                "building".hash(state);
                building_idx.hash(state);
            }
            Location::Floor(building_idx, floor_idx) => {
                "floor".hash(state);
                building_idx.hash(state);
                floor_idx.hash(state);
            }
            Location::Room(building_idx, floor_idx, room_idx) => {
                "room".hash(state);
                building_idx.hash(state);
                floor_idx.hash(state);
                room_idx.hash(state);
            }
        }
    }
}

impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Location::Building(building_idx), Location::Building(other_building_idx)) => {
                building_idx == other_building_idx
            }
            (
                Location::Floor(building_idx, floor_idx),
                Location::Floor(other_building_idx, other_floor_idx),
            ) => building_idx == other_building_idx && floor_idx == other_floor_idx,
            (
                Location::Room(building_idx, floor_idx, room_idx),
                Location::Room(other_building_idx, other_floor_idx, other_room_idx),
            ) => {
                building_idx == other_building_idx
                    && floor_idx == other_floor_idx
                    && room_idx == other_room_idx
            }
            _ => false,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Building(building_idx) => write!(f, "b_{}", building_idx),
            Location::Floor(building_idx, floor_idx) => {
                write!(f, "f_{}_{}", building_idx, floor_idx)
            }
            Location::Room(building_idx, floor_idx, room_idx) => {
                write!(f, "r_{}_{}_{}", building_idx, floor_idx, room_idx)
            }
        }
    }
}

pub enum RoomProperty {
    Temperature,
    Humidity,
    Co2,
}

#[derive(Debug)]
struct Building {
    effective_from: u64,
    source_id: String,
    location: Location,
}

impl Building {
    fn new(effective_from: u64, source_id: String, location: Location) -> Building {
        // Validate that the provided Building is a Floor Location or throw error
        if let Location::Building(_) = location {
            Building {
                effective_from,
                source_id: source_id,
                location,
            }
        } else {
            panic!("Invalid location provided to Building::new().");
        }
    }

    fn from_domain_graph_node(node: Node) -> Building {
        Building {
            effective_from: node.effective_from,
            source_id: node.source_id,
            location: Location::from_string(&node.id),
        }
    }

    fn id(&self) -> String {
        self.location.to_string()
    }

    fn name(&self) -> String {
        format!("Building {}", self.location.get_building_idx())
    }

    fn element_reference(&self) -> ElementReference {
        ElementReference::new(&self.source_id, &self.id())
    }

    fn to_domain_graph_node(&self) -> Node {
        let mut node = Node::new(
            self.id(),
            self.effective_from,
            self.source_id.clone(),
            vec!["Building".to_string()],
            BTreeMap::new(),
        );

        // Add the room properties to the Node
        node.properties
            .insert("name".to_string(), VariableValue::String(self.name()));

        node
    }
}

#[derive(Debug)]
struct Floor {
    effective_from: u64,
    source_id: String,
    location: Location,
}

impl Floor {
    fn new(effective_from: u64, source_id: String, location: Location) -> Floor {
        // Validate that the provided Location is a Floor Location or throw error
        if let Location::Floor(_, _) = location {
            Floor {
                effective_from,
                source_id: source_id,
                location,
            }
        } else {
            panic!("Invalid location provided to Floor::new().");
        }
    }

    fn from_domain_graph_node(node: Node) -> Floor {
        Floor {
            effective_from: node.effective_from,
            source_id: node.source_id,
            location: Location::from_string(&node.id),
        }
    }

    fn id(&self) -> String {
        self.location.to_string()
    }

    fn name(&self) -> String {
        format!("Floor {}", self.location.to_string())
    }

    fn to_domain_graph_node(&self) -> Node {
        let mut node = Node::new(
            self.id(),
            self.effective_from,
            self.source_id.clone(),
            vec!["Floor".to_string()],
            BTreeMap::new(),
        );

        // Add the room properties to the Node
        node.properties
            .insert("name".to_string(), VariableValue::String(self.name()));

        node
    }
}

#[derive(Debug)]
struct Room {
    source_id: String,
    location: Location,
    effective_from: RwLock<u64>,
    temperature: RwLock<f32>,
    humidity: RwLock<f32>,
    co2: RwLock<f32>,
}

impl Room {
    fn new(effective_from: u64, source_id: String, location: Location) -> Room {
        // Validate that the provided Location is a Room Location or throw error
        if let Location::Room(_, _, _) = location {
            Room {
                source_id: source_id,
                location,
                effective_from: RwLock::new(effective_from),
                temperature: RwLock::new(72.0),
                humidity: RwLock::new(42.0),
                co2: RwLock::new(500.0),
            }
        } else {
            panic!("Invalid location provided to Room::new().");
        }
    }

    fn from_domain_graph_node(node: Node) -> Room {
        Room {
            source_id: "bootstrap".to_string(),
            location: Location::from_string(&node.id),
            effective_from: RwLock::new(node.effective_from),
            temperature: RwLock::new(
                node.properties
                    .get("temperature")
                    .unwrap()
                    .as_f64()
                    .unwrap() as f32,
            ),
            humidity: RwLock::new(node.properties.get("humidity").unwrap().as_f64().unwrap() as f32),
            co2: RwLock::new(node.properties.get("co2").unwrap().as_f64().unwrap() as f32),
        }
    }

    fn id(&self) -> String {
        self.location.to_string()
    }

    fn name(&self) -> String {
        format!("Room {}_", self.location.to_string())
    }

    fn element_reference(&self) -> ElementReference {
        ElementReference::new(&self.source_id, &self.id())
    }

    fn update_temperature(&self, effective_from: u64, temperature_delta: f32) {
        // Add temperature delta to current temperature
        let mut e = self.effective_from.write().unwrap();
        {
            let mut t = self.temperature.write().unwrap();
            *t += temperature_delta;
        }
        *e = effective_from;
    }

    fn update_humidity(&self, effective_from: u64, humidity_delta: f32) {
        let mut e = self.effective_from.write().unwrap();
        {
            let mut h = self.humidity.write().unwrap();
            *h += humidity_delta;
        }
        *e = effective_from;
    }

    fn update_co2(&self, effective_from: u64, co2_delta: f32) {
        let mut e = self.effective_from.write().unwrap();
        {
            let mut c = self.co2.write().unwrap();
            *c += co2_delta;
        }
        *e = effective_from;
    }

    pub fn get_comfort_level(&self) -> f32 {
        let mut comfort_level = match *self.co2.read().unwrap() {
            co2 if co2 > 500.0 => (co2 - 500.0) / 25.0,
            _ => 0.0,
        };
        comfort_level = comfort_level
            + 50.0
            + (*self.temperature.read().unwrap() - 72.0)
            + (*self.humidity.read().unwrap() - 42.0);

        comfort_level
    }

    fn to_domain_graph_node(&self) -> Node {
        let mut node = Node::new(
            self.id(),
            *self.effective_from.read().unwrap(),
            self.source_id.clone(),
            vec!["Room".to_string()],
            BTreeMap::new(),
        );

        // Add the room properties to the Node
        node.properties
            .insert("name".to_string(), VariableValue::String(self.name()));
        node.properties.insert(
            "temperature".to_string(),
            VariableValue::Float(Float::from(*self.temperature.read().unwrap())),
        );
        node.properties.insert(
            "humidity".to_string(),
            VariableValue::Float(Float::from(*self.humidity.read().unwrap())),
        );
        node.properties.insert(
            "co2".to_string(),
            VariableValue::Float(Float::from(*self.co2.read().unwrap())),
        );

        node
    }
}

pub struct PartOfRelationship {
    effective_from: u64,
    source_id: String,
    from_id: NodeId,
    to_id: NodeId,
}

impl PartOfRelationship {
    pub fn new(
        effective_from: u64,
        source_id: &str,
        from_id: NodeId,
        to_id: NodeId,
    ) -> PartOfRelationship {
        PartOfRelationship {
            effective_from,
            source_id: source_id.to_string(),
            from_id,
            to_id,
        }
    }

    pub fn from_domain_graph_relation(relation: Relation) -> PartOfRelationship {
        PartOfRelationship {
            effective_from: relation.effective_from,
            source_id: relation.source_id,
            from_id: relation.from_id,
            to_id: relation.to_id,
        }
    }

    pub fn id(&self) -> String {
        format!("po_{}_{}", self.from_id, self.to_id)
    }

    pub fn element_reference(&self) -> ElementReference {
        ElementReference::new(&self.source_id, &self.id())
    }

    pub fn to_change_element(&self) -> Element {
        Element::Relation {
            metadata: ElementMetadata {
                reference: self.element_reference(),
                labels: Arc::new([Arc::from("PART_OF")]),
                effective_from: self.effective_from,
            },
            in_node: ElementReference::new(&self.source_id, &self.from_id),
            out_node: ElementReference::new(&self.source_id, &self.to_id),
            properties: ElementPropertyMap::new(),
        }
    }

    pub fn to_domain_graph_relation(&self) -> Relation {
        Relation::new(
            self.id(),
            self.effective_from,
            self.source_id.clone(),
            vec!["PART_OF".to_string()],
            self.from_id.clone(),
            self.to_id.clone(),
        )
    }
}

pub struct BuildingComfortModel {
    current_building_sizes: Vec<(usize, usize, usize)>,
    last_change_time_ms: u64,
    graph: RwLock<DomainModelGraph>,
}

impl BuildingComfortModel {
    pub fn new(start_time_ms: u64) -> BuildingComfortModel {
        BuildingComfortModel {
            current_building_sizes: Vec::new(),
            last_change_time_ms: start_time_ms,
            graph: RwLock::new(DomainModelGraph::new()),
        }
    }

    // The add_building method adds a building to the BuildingComfortModel.
    pub fn add_building_hierarchy(
        &mut self,
        source_id: String,
        effective_from_ms: u64,
        time_step_ms: u64,
        floor_count: usize,
        rooms_per_floor: usize,
    ) {
        // println!("add_building: effective_from: {effective_from_ms}, floor_count: {floor_count}, rooms_per_floor: {rooms_per_floor}");

        // If the effective_from_ms time is less than the model's last_change_time_ms, error.
        if effective_from_ms < self.last_change_time_ms {
            panic!("BuildingComfortModel::add_building() called with an effective_from time ({}) that is less than the last_change_time_ms ({})", effective_from_ms, self.last_change_time_ms);
        }

        // Get mutable access to the DomainModelGraph.
        let mut graph = self.graph.write().unwrap();

        // Start working from the effective_from_ms time.
        let mut current_time_ms = effective_from_ms;

        let new_building_idx = self.current_building_sizes.len();
        self.current_building_sizes
            .push((new_building_idx, floor_count, rooms_per_floor));

        // Build the domain model; a hierarchy of buildings, floors, and rooms.
        let building = Building::new(
            current_time_ms,
            source_id.clone(),
            Location::Building(new_building_idx),
        );

        // Create a Node for the building and add it to the DomainModelGraph
        let mut node_result = graph.add_node(building.to_domain_graph_node());
        if node_result.is_err() {
            panic!("Error returned from DomainModelGraph::add_node() adding building.");
        }

        // Loop from 0 to the number of floors in the building, create the floors, and add them to the DomainModelGraph.
        for floor_idx in 0..floor_count {
            current_time_ms += time_step_ms;

            let floor = Floor::new(
                current_time_ms,
                source_id.clone(),
                Location::new_floor_from_building_location(building.location, floor_idx),
            );
            // println!("{:#?}", floor.location);

            // Create a Node for the floor and add it to the DomainModelGraph
            node_result = graph.add_node(floor.to_domain_graph_node());
            if node_result.is_err() {
                panic!(
                    "Error returned from DomainModelGraph::add_node() adding floor_idx: {}.",
                    floor_idx
                );
            }

            // Create a PART_OF relationship between the floor and the building and add it to the DomainModelGraph
            let mut rel_result = graph.add_relation(
                PartOfRelationship::new(current_time_ms, &source_id, floor.id(), building.id())
                    .to_domain_graph_relation(),
            );
            if rel_result.is_err() {
                panic!("Error returned from DomainModelGraph::add_relation() adding PartOfRelationship from floor:{} to building:{}.", floor.id(), building.id());
            }

            // Loop from 0 to the number of rooms per floor, create the rooms, and add them to the DomainModelGraph.
            for room_idx in 0..rooms_per_floor {
                current_time_ms += time_step_ms;

                let room = Room::new(
                    self.last_change_time_ms,
                    source_id.clone(),
                    Location::new_room_from_floor_location(floor.location, room_idx),
                );
                // println!("{:#?}", room.location);

                // Add the room to the DomainModelGraph
                node_result = graph.add_node(room.to_domain_graph_node());
                if node_result.is_err() {
                    panic!(
                        "Error returned from DomainModelGraph::add_node() adding room_idx: {}.",
                        room_idx
                    );
                }

                // Create a PART_OF relationship between the room and the floor and add it to the DomainModelGraph
                rel_result = graph.add_relation(
                    PartOfRelationship::new(current_time_ms, &source_id, room.id(), floor.id())
                        .to_domain_graph_relation(),
                );
                if rel_result.is_err() {
                    panic!("Error returned from DomainModelGraph::add_relation() adding PartOfRelationship from room:{} to floor:{}.", room.id(), floor.id());
                }
            }

            self.last_change_time_ms = current_time_ms;
        }
    }

    pub fn get_building_count(&self) -> usize {
        self.current_building_sizes.len()
    }

    pub fn get_building_size(&self, building_idx: usize) -> (usize, usize) {
        let (_, floor_count, rooms_per_floor) = self.current_building_sizes[building_idx];
        (floor_count, rooms_per_floor)
    }

    pub fn get_last_change_time_ms(&self) -> u64 {
        self.last_change_time_ms
    }

    pub fn update_room_property(
        &self,
        effective_from: u64,
        room_location: Location,
        property: RoomProperty,
        property_delta: f32,
    ) -> ModelChangeResult {
        // If the effective_from time is less than the last_change_time_ms, throw an error.
        if effective_from < self.last_change_time_ms {
            panic!("BuildingComfortModel::update_room_property() called with an effective_from time that is less than the last_change_time_ms.");
        }

        // Validate that the provided Location is a Room Location or throw error
        let mut graph = self.graph.write().unwrap();

        if let Location::Room(_, _, _) = room_location {
            let room_node_before = graph.get_node(room_location.to_string());

            // if room was found, update the named property
            if let Ok(room_node_before) = room_node_before {
                let property_name = match property {
                    RoomProperty::Temperature => "temperature".to_string(),
                    RoomProperty::Humidity => "humidity".to_string(),
                    RoomProperty::Co2 => "co2".to_string(),
                };

                let mut room_node_after = room_node_before.clone();
                room_node_after.properties.insert(
                    property_name.clone(),
                    VariableValue::Float(Float::from(
                        room_node_before
                            .properties
                            .get(&property_name)
                            .unwrap()
                            .as_f64()
                            .unwrap() as f32
                            + property_delta,
                    )),
                );
                room_node_after.effective_from = effective_from;

                let result = graph.upsert_node(room_node_after);

                // if result is not an error, return the ModelChangeResult
                match result {
                    Ok(result) => match result {
                        GraphChangeResult::Insert { element } => {
                            ModelChangeResult::new_insert_node(element.into_element())
                        }
                        GraphChangeResult::Update {
                            element_before,
                            element_after,
                        } => ModelChangeResult::new_update_node(
                            element_before.into_element(),
                            element_after.into_element(),
                        ),
                        GraphChangeResult::Delete { element } => {
                            ModelChangeResult::new_delete_node(element.into_element())
                        }
                    },
                    Err(_) => {
                        panic!("Error returned from DomainModelGraph::upsert_node().");
                    }
                }
            } else {
                panic!(
                    "Invalid location provided to BuildingComfortModel::update_room_property()."
                );
            }
        } else {
            panic!("Invalid location provided to BuildingComfortModel::update_room_property().");
        }
    }
}

pub struct BootstrapSourceChangeGenerator {
    model: Arc<BuildingComfortModel>,
    graph_ids: GraphIds,
}

impl BootstrapSourceChangeGenerator {
    pub fn new(model: Arc<BuildingComfortModel>) -> BootstrapSourceChangeGenerator {
        // Get all NodeIds and RelationIds from the DomainModelGraph calling get_graph_ids_by_filter()
        let graph_ids = model
            .graph
            .read()
            .unwrap()
            .get_graph_ids_by_filter(GraphFilter::All, GraphFilter::All);

        BootstrapSourceChangeGenerator { model, graph_ids }
    }
}

// Implementation of SourceChangeGenerator for BootstrapSourceChangeGenerator. When a call is made
// to generate_change(), a NodeId is removed from the node_ids Vec and the corresponding Node is
// read from the DomainModelGraph. The Node is then converted to an Element and returned as a new
// SourceChange::Insert. Once all the NodeIds have been removed from the node_ids Vec, the same process
// is repeated for the relation_ids Vec. Once all the NodeIds and RelationIds have been removed from
// their respective Vecs, None is returned to indicate that no more SourceChanges are available.
impl SourceChangeGenerator for BootstrapSourceChangeGenerator {
    fn generate_change(&mut self) -> Option<SourceChange> {
        // If there are any NodeIds left in the node_ids Vec, remove the first NodeId and return a
        // SourceChange::Insert for the corresponding Node.
        if let Some(node_id) = self.graph_ids.node_ids.pop() {
            let node = self.model.graph.read().unwrap().get_node(node_id).unwrap();
            let element = node.into_element();
            return Some(SourceChange::Insert { element });
        }

        // If there are any RelationIds left in the relation_ids Vec, remove the first RelationId
        // and return a SourceChange::Insert for the corresponding Relation.
        if let Some(relation_id) = self.graph_ids.relation_ids.pop() {
            let relation = self
                .model
                .graph
                .read()
                .unwrap()
                .get_relation(relation_id.clone())
                .unwrap();
            let element = relation.into_element();
            return Some(SourceChange::Insert { element });
        }

        // If there are no more NodeIds or RelationIds left in their respective Vecs, return None
        // to indicate that no more SourceChanges are available.
        None
    }
}
