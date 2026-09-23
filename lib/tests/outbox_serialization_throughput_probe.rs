//! Diagnostic for plugin-facing QueryResult encoding. Computation outboxes persist
//! typed envelopes rather than either of the MessagePack representations below.

use std::{collections::HashMap, hint::black_box, time::Instant};

use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    profiling::ProfilingMetadata,
};
use serde_json::json;

#[test]
#[ignore = "diagnostic plugin serialization timing; not a computation outbox or recovery baseline"]
#[allow(
    clippy::print_stdout,
    reason = "diagnostic probe emits CSV timing results"
)]
fn measure_plugin_result_encoding_cost() {
    let iterations = 100000;
    let room_before =
        json!({"RoomId":"R_000_001_000","Temperature":5110,"Humidity":5061,"Co2":4769});
    let room_after =
        json!({"RoomId":"R_000_001_000","Temperature":5111,"Humidity":5061,"Co2":4769});
    let floor_before = json!({"FloorId":"F_000_001","AvgTemperature":4973.5,"MaxCo2":5053.0,"MinHumidity":4809.0,"RoomCount":4});
    let floor_after = json!({"FloorId":"F_000_001","AvgTemperature":4973.75,"MaxCo2":5053.0,"MinHumidity":4809.0,"RoomCount":4});
    let cases = [
        (
            "room-update",
            ResultDiff::Update {
                data: room_after.clone(),
                before: room_before,
                after: room_after,
                grouping_keys: None,
                row_signature: 13660005145781501189,
            },
        ),
        (
            "floor-update",
            ResultDiff::Update {
                data: floor_after.clone(),
                before: floor_before.clone(),
                after: floor_after.clone(),
                grouping_keys: None,
                row_signature: 8918764620589370281,
            },
        ),
        (
            "aggregation",
            ResultDiff::Aggregation {
                before: Some(floor_before),
                after: floor_after,
                row_signature: 8918764620589370281,
            },
        ),
    ];
    println!("case,round,encoding,iterations,encoded_bytes,encode_us");
    for (label, diff) in cases {
        let result = QueryResult::with_profiling(
            "building-comfort".to_owned(),
            99900,
            chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
            vec![diff],
            HashMap::from([
                ("source".to_owned(), json!("drasi-core")),
                ("result_count".to_owned(), json!(1)),
            ]),
            ProfilingMetadata::new(),
        );
        let named = rmp_serde::to_vec_named(&result).unwrap();
        let restored: QueryResult = rmp_serde::from_slice(&named).unwrap();
        assert_eq!(
            serde_json::to_value(&restored).unwrap(),
            serde_json::to_value(&result).unwrap()
        );
        for round in 0..3 {
            let encodings = if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for named in encodings {
                for _ in 0..1000 {
                    black_box(
                        if named {
                            rmp_serde::to_vec_named(black_box(&result))
                        } else {
                            rmp_serde::to_vec(black_box(&result))
                        }
                        .unwrap(),
                    );
                }
                let start = Instant::now();
                let mut bytes = 0;
                for _ in 0..iterations {
                    let encoded = if named {
                        rmp_serde::to_vec_named(black_box(&result))
                    } else {
                        rmp_serde::to_vec(black_box(&result))
                    }
                    .unwrap();
                    bytes = encoded.len();
                    black_box(encoded);
                }
                println!(
                    "{label},{round},{},{iterations},{bytes},{:.4}",
                    if named { "named" } else { "positional" },
                    start.elapsed().as_secs_f64() * 1e6 / iterations as f64
                );
            }
        }
    }
}
