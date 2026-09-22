use std::sync::Arc;
use std::time::{Duration, Instant};

use drasi_core::interface::OutboxWriter;
use drasi_index_rocksdb::{
    open_unified_db, RocksDbMemoryBudget, RocksDbOutboxWriter, RocksDbSessionState,
    RocksIndexOptions,
};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "diagnostic timing probe; run in release mode with --nocapture"]
async fn measure_outbox_retention_cost() {
    let iterations = 200;
    println!("round,capacity,payload_bytes,iterations,scan_only_us,append_us,trim_us,append_trim_ops_per_second");
    for round in 0..3 {
        let capacities = if round % 2 == 0 {
            [1000, 5000, 20000]
        } else {
            [20000, 5000, 1000]
        };
        for payload_bytes in [283, 570, 388, 675] {
            for capacity in capacities {
                let directory = TempDir::new().unwrap();
                let options = RocksIndexOptions::new(false, false, RocksDbMemoryBudget::default());
                let db =
                    open_unified_db(directory.path().to_str().unwrap(), "probe", &options).unwrap();
                let session = Arc::new(RocksDbSessionState::new(db.clone()));
                let writer = RocksDbOutboxWriter::new(db, session);
                let payload: Vec<u8> = (0..payload_bytes)
                    .map(|offset| (offset % 251) as u8)
                    .collect();
                for sequence in 1..=capacity as u64 {
                    writer.append("probe", sequence, &payload).await.unwrap();
                }
                for _ in 0..10 {
                    assert_eq!(writer.trim_to_capacity("probe", capacity).await.unwrap(), 0);
                }
                let scan_start = Instant::now();
                for _ in 0..iterations {
                    assert_eq!(writer.trim_to_capacity("probe", capacity).await.unwrap(), 0);
                }
                let scan_duration = scan_start.elapsed();
                let mut append_duration = Duration::ZERO;
                let mut trim_duration = Duration::ZERO;
                for offset in 1..=iterations {
                    let start = Instant::now();
                    writer
                        .append("probe", capacity as u64 + offset, &payload)
                        .await
                        .unwrap();
                    append_duration += start.elapsed();
                    let start = Instant::now();
                    assert_eq!(writer.trim_to_capacity("probe", capacity).await.unwrap(), 1);
                    trim_duration += start.elapsed();
                }
                let entries = writer.read_from("probe", 0).await.unwrap();
                assert_eq!(entries.len(), capacity);
                assert_eq!(entries.first().unwrap().0, iterations + 1);
                assert_eq!(entries.last().unwrap().0, capacity as u64 + iterations);
                assert!(entries.iter().all(|(_, bytes)| bytes == &payload));
                let micros = |duration: Duration| duration.as_secs_f64() * 1e6 / iterations as f64;
                println!(
                    "{round},{capacity},{payload_bytes},{iterations},{:.3},{:.3},{:.3},{:.1}",
                    micros(scan_duration),
                    micros(append_duration),
                    micros(trim_duration),
                    iterations as f64 / (append_duration + trim_duration).as_secs_f64()
                );
            }
        }
    }
}
