// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_contracts::telemetry::{
    HandledBranchKey, HandledBranchLimits, HandledBranchTracker,
};
use std::{process::Command, time::Instant};

fn rss_kib() -> Result<u64, Box<dyn std::error::Error>> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()?;
    if !output.status.success() {
        return Err("ps could not read this process's resident memory".into());
    }
    Ok(std::str::from_utf8(&output.stdout)?.trim().parse()?)
}

fn key(sequence: u64) -> HandledBranchKey {
    HandledBranchKey {
        producer_epoch: [1; 16],
        output_sequence: sequence,
        sink: "sink0000".into(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let limits = HandledBranchLimits::default();
    let tracker = HandledBranchTracker::new(limits)?;
    let baseline = rss_kib()?;
    let origin = tracker.start()?;
    let token = origin.token().ok_or("missing timing token")?;
    let started = Instant::now();
    for sequence in 0..(limits.max_records - 2) as u64 {
        tracker.prepare(token, &key(sequence), 1)?;
    }
    let elapsed = started.elapsed();
    let counts = tracker.diagnostics()?;
    assert_eq!(counts.records, limits.max_records);
    assert!(tracker
        .prepare(token, &key(limits.max_records as u64), 1)
        .is_err());
    let count_rss = rss_kib()?;
    println!("count limit: records={}, record_bytes={}, rss_kib={}, baseline_rss_kib={}, admission_ns_per_branch={}",
        counts.records, counts.record_bytes, count_rss, baseline,
        elapsed.as_nanos() / (limits.max_records - 2) as u128);

    drop(origin);
    assert_eq!(tracker.diagnostics()?.records, 0);
    let probe = tracker.start()?;
    let token = probe.token().ok_or("missing probe token")?;
    let origin_bytes = tracker.diagnostics()?.record_bytes;
    tracker.prepare(token, &key(1), 1)?;
    let fixed_branch_bytes = tracker.diagnostics()?.record_bytes - origin_bytes - 8;
    drop(probe);

    let origin = tracker.start()?;
    let token = origin.token().ok_or("missing byte-limit token")?;
    let row_bytes = limits.max_bytes - origin_bytes - fixed_branch_bytes;
    assert_eq!(row_bytes % 8, 0);
    let rows = row_bytes / 8 * 64;
    let branch = key(1);
    tracker.prepare(token, &branch, rows)?;
    // Fault every bitset page in without allocating a second large ordinal list.
    for ordinal in (0..rows).step_by(64) {
        tracker.handled_rows(token, &branch, &[ordinal])?;
    }
    let bytes = tracker.diagnostics()?;
    assert_eq!(bytes.record_bytes, limits.max_bytes);
    assert!(tracker.prepare(token, &key(2), 1).is_err());
    println!(
        "byte limit: records={}, record_bytes={}, rss_kib={}, baseline_rss_kib={}",
        bytes.records,
        bytes.record_bytes,
        rss_kib()?,
        baseline
    );
    drop(origin);
    assert_eq!(tracker.diagnostics()?.records, 0);
    assert_eq!(tracker.diagnostics()?.record_bytes, 0);
    Ok(())
}
