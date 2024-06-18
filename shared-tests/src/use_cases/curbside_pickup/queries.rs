use drasi_core::models::{QueryJoin, QueryJoinKey};

pub fn pickup_order_ready_query() -> &'static str {
    "
  MATCH 
    (o:Order {status:'ready'})<-[:PICKUP_ORDER]-(:OrderPickup)-[:PICKUP_DRIVER]->(d:Driver)-[:VEHICLE_TO_DRIVER]->(v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Curbside Queue'}) 
    RETURN elementId(o) AS OrderNumber, d.name AS DriverName, v.licensePlate AS LicensePlate
    "
}

pub fn pickup_order_ready_metadata() -> Vec<QueryJoin> {
    vec![QueryJoin {
        id: "VEHICLE_TO_DRIVER".into(),
        keys: vec![
            QueryJoinKey {
                label: "Vehicle".into(),
                property: "licensePlate".into(),
            },
            QueryJoinKey {
                label: "Driver".into(),
                property: "vehicleLicensePlate".into(),
            },
        ],
    }]
}
