// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

/// Helper to create test profiling metadata with specific timestamps
fn create_profiling_metadata(
    source_send_ns: Option<u64>,
    query_receive_ns: Option<u64>,
    query_core_call_ns: Option<u64>,
    query_core_return_ns: Option<u64>,
    query_send_ns: Option<u64>,
    reaction_receive_ns: Option<u64>,
    reaction_complete_ns: Option<u64>,
) -> ProfilingMetadata {
    ProfilingMetadata {
        source_ns: None,
        reactivator_start_ns: None,
        reactivator_end_ns: None,
        source_receive_ns: None,
        source_send_ns,
        query_receive_ns,
        query_core_call_ns,
        query_core_return_ns,
        query_send_ns,
        reaction_receive_ns,
        reaction_complete_ns,
    }
}

#[test]
fn test_welford_algorithm_single_sample() {
    let mut stats = ProfilingStats::new(1000);

    let profiling = create_profiling_metadata(
        Some(1000),
        Some(2000),
        Some(3000),
        Some(4000),
        Some(5000),
        Some(6000),
        Some(7000),
    );

    stats.add_sample(profiling);

    assert_eq!(stats.count, 1);
    assert_eq!(stats.mean_source_to_query, 1000.0);
    assert_eq!(stats.mean_query_processing, 1000.0);
    assert_eq!(stats.mean_query_to_reaction, 1000.0);
    assert_eq!(stats.mean_reaction_processing, 1000.0);
    assert_eq!(stats.mean_total_latency, 6000.0);

    // With single sample, variance should be 0
    let source_stats = stats.get_source_to_query_stats();
    assert_eq!(source_stats.variance, 0.0);
    assert_eq!(source_stats.std_dev, 0.0);
}

#[test]
fn test_welford_algorithm_multiple_samples() {
    let mut stats = ProfilingStats::new(1000);

    // Add samples: 1000, 2000, 3000
    for i in 1..=3 {
        let base = i as u64 * 1000;
        let profiling = create_profiling_metadata(
            Some(0),
            Some(base),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    assert_eq!(stats.count, 3);
    // Mean should be (1000 + 2000 + 3000) / 3 = 2000
    assert_eq!(stats.mean_source_to_query, 2000.0);

    let source_stats = stats.get_source_to_query_stats();

    // Sample variance: ((1000-2000)^2 + (2000-2000)^2 + (3000-2000)^2) / (3-1)
    // = (1000000 + 0 + 1000000) / 2 = 1000000
    assert_eq!(source_stats.variance, 1000000.0);

    // Standard deviation should be sqrt(1000000) = 1000
    assert_eq!(source_stats.std_dev, 1000.0);
}

#[test]
fn test_welford_algorithm_numerical_stability() {
    let mut stats = ProfilingStats::new(1000);

    // Test with large numbers that would cause issues with naive variance calculation
    let large_base = 1_000_000_000_000_u64; // 1 trillion nanoseconds

    for i in 0..100 {
        let offset = i * 1000;
        let profiling = create_profiling_metadata(
            Some(large_base),
            Some(large_base + offset + 1000), // Ensure query_receive > source_send
            None,                             // Don't set later timestamps to avoid underflow
            None,
            None,
            None,
            None,
        );
        stats.add_sample(profiling);
    }

    let source_stats = stats.get_source_to_query_stats();

    // Mean should be around 50500 (1000 + 2000 + 3000 + ... + 100000) / 100
    assert!((source_stats.mean - 50500.0).abs() < 1.0);

    // Variance should be computed correctly despite large base
    assert!(source_stats.variance > 0.0);
    assert!(source_stats.std_dev > 0.0);
}

#[test]
fn test_percentile_calculation_basic() {
    let mut stats = ProfilingStats::new(1000);

    // Add 100 samples with known distribution
    for i in 0..100 {
        let val = i * 100; // 0, 100, 200, ..., 9900 nanoseconds
        let profiling = create_profiling_metadata(
            Some(0),
            Some(val),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    let source_stats = stats.get_source_to_query_stats();

    // Min should be 0
    assert_eq!(source_stats.min, 0.0);

    // Max should be 9900
    assert_eq!(source_stats.max, 9900.0);

    // P50 (median) should be around 4950 (middle value)
    // Index = 0.50 * (100-1) = 49.5 -> rounds to 50
    assert!((source_stats.p50 - 4950.0).abs() < 100.0);

    // P95 should be around 9405
    // Index = 0.95 * (100-1) = 94.05 -> rounds to 94
    assert!((source_stats.p95 - 9400.0).abs() < 100.0);

    // P99 should be around 9801
    // Index = 0.99 * (100-1) = 98.01 -> rounds to 98
    assert!((source_stats.p99 - 9800.0).abs() < 100.0);
}

#[test]
fn test_percentile_with_duplicates() {
    let mut stats = ProfilingStats::new(1000);

    // Add many samples with same value
    for _ in 0..50 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(1000),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    // Add some different values
    for i in 0..10 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(2000 + i * 100),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    let source_stats = stats.get_source_to_query_stats();

    // P50 should be close to 1000 since most values are 1000
    assert_eq!(source_stats.p50, 1000.0);
}

#[test]
fn test_sliding_window_behavior() {
    let window_size = 10;
    let mut stats = ProfilingStats::new(window_size);

    // Add more samples than window size
    for i in 0..20 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(i * 1000),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    // Window should only contain last 10 samples
    assert_eq!(stats.samples.len(), window_size);

    // Count should be total samples (20), not window size (10)
    assert_eq!(stats.count, 20);

    let source_stats = stats.get_source_to_query_stats();

    // Min should be from the last 10 samples: 10000 (index 10)
    assert_eq!(source_stats.min, 10000.0);

    // Max should be from the last 10 samples: 19000 (index 19)
    assert_eq!(source_stats.max, 19000.0);

    // But mean should include all 20 samples
    // (0 + 1000 + 2000 + ... + 19000) / 20 = 9500
    assert_eq!(stats.mean_source_to_query, 9500.0);
}

#[test]
fn test_sliding_window_percentiles_update() {
    let window_size = 5;
    let mut stats = ProfilingStats::new(window_size);

    // Add first batch: 1000, 2000, 3000, 4000, 5000
    for i in 1..=5 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(i * 1000),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    let stats1 = stats.get_source_to_query_stats();
    assert_eq!(stats1.min, 1000.0);
    assert_eq!(stats1.max, 5000.0);

    // Add second batch: 10000, 20000, 30000, 40000, 50000
    // This should push out the first 5 samples
    for i in 1..=5 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(i * 10000),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    let stats2 = stats.get_source_to_query_stats();

    // Percentiles should reflect only the last 5 samples
    assert_eq!(stats2.min, 10000.0);
    assert_eq!(stats2.max, 50000.0);

    // But mean should include all 10 samples
    // (1000+2000+3000+4000+5000 + 10000+20000+30000+40000+50000) / 10 = 16500
    assert_eq!(stats.mean_source_to_query, 16500.0);
}

#[test]
fn test_multiple_metrics_independence() {
    let mut stats = ProfilingStats::new(1000);

    // Add samples with different values for different metrics
    let profiling = create_profiling_metadata(
        Some(0),
        Some(1000), // source_to_query = 1000
        Some(0),
        Some(5000), // query_processing = 5000
        Some(0),
        Some(2000),  // query_to_reaction = 2000
        Some(10000), // reaction_processing = 8000
    );

    stats.add_sample(profiling);

    assert_eq!(stats.mean_source_to_query, 1000.0);
    assert_eq!(stats.mean_query_processing, 5000.0);
    assert_eq!(stats.mean_query_to_reaction, 2000.0);
    assert_eq!(stats.mean_reaction_processing, 8000.0);

    let source_stats = stats.get_source_to_query_stats();
    let query_stats = stats.get_query_processing_stats();
    let reaction_stats = stats.get_reaction_processing_stats();

    assert_eq!(source_stats.mean, 1000.0);
    assert_eq!(query_stats.mean, 5000.0);
    assert_eq!(reaction_stats.mean, 8000.0);
}

#[test]
fn test_missing_timestamps() {
    let mut stats = ProfilingStats::new(1000);

    // Add sample with missing timestamps
    let profiling = create_profiling_metadata(
        None, // Missing
        Some(1000),
        Some(2000),
        Some(3000),
        None, // Missing
        Some(4000),
        Some(5000),
    );

    stats.add_sample(profiling);

    assert_eq!(stats.count, 1);

    // Metrics with missing timestamps should remain at initial value (0.0)
    assert_eq!(stats.mean_source_to_query, 0.0);
    assert_eq!(stats.mean_query_to_reaction, 0.0);

    // Metrics with complete timestamps should be calculated
    assert_eq!(stats.mean_query_processing, 1000.0);
    assert_eq!(stats.mean_reaction_processing, 1000.0);
}

#[test]
fn test_all_metrics_comprehensive() {
    let mut stats = ProfilingStats::new(1000);

    // Add a complete profiling sample with continuous timestamps (no gaps)
    let profiling = create_profiling_metadata(
        Some(1000),
        Some(2000), // source_to_query = 1000
        Some(2000), // Start query processing immediately
        Some(4000), // query_processing = 2000
        Some(4000), // Send immediately after query processing
        Some(5000), // query_to_reaction = 1000
        Some(8000), // reaction_processing = 3000
    );

    stats.add_sample(profiling);

    let source_to_query = stats.get_source_to_query_stats();
    let query_processing = stats.get_query_processing_stats();
    let query_to_reaction = stats.get_query_to_reaction_stats();
    let reaction_processing = stats.get_reaction_processing_stats();
    let total = stats.get_total_latency_stats();

    // Verify each metric
    assert_eq!(source_to_query.mean, 1000.0); // 2000 - 1000
    assert_eq!(query_processing.mean, 2000.0); // 4000 - 2000
    assert_eq!(query_to_reaction.mean, 1000.0); // 5000 - 4000
    assert_eq!(reaction_processing.mean, 3000.0); // 8000 - 5000
    assert_eq!(total.mean, 7000.0); // 8000 - 1000

    // With no gaps, total should equal sum of parts exactly
    let sum_of_parts = source_to_query.mean
        + query_processing.mean
        + query_to_reaction.mean
        + reaction_processing.mean;
    assert_eq!(total.mean, sum_of_parts);
}

#[test]
fn test_percentile_function_edge_cases() {
    // Test with single value
    let values = vec![100.0];
    assert_eq!(percentile(&values, 0.50), 100.0);
    assert_eq!(percentile(&values, 0.95), 100.0);
    assert_eq!(percentile(&values, 0.99), 100.0);

    // Test with two values
    let values = vec![100.0, 200.0];
    assert_eq!(percentile(&values, 0.50), 200.0); // rounds to index 1

    // Test with exact percentile boundaries
    let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    assert_eq!(percentile(&values, 0.00), 1.0); // min
    assert_eq!(percentile(&values, 1.00), 5.0); // max
}

#[test]
fn test_empty_samples_percentiles() {
    let stats = ProfilingStats::new(1000);

    // With no samples, percentiles should return 0.0
    let source_stats = stats.get_source_to_query_stats();
    assert_eq!(source_stats.min, 0.0);
    assert_eq!(source_stats.max, 0.0);
    assert_eq!(source_stats.p50, 0.0);
    assert_eq!(source_stats.p95, 0.0);
    assert_eq!(source_stats.p99, 0.0);
}

#[test]
fn test_metric_stats_structure() {
    let mut stats = ProfilingStats::new(1000);

    // Add several samples to get meaningful statistics
    // Use monotonically increasing timestamps
    for i in 0..10 {
        let base = i * 10000;
        let profiling = create_profiling_metadata(
            Some(base),
            Some(base + 1000 + i * 100), // query_receive > source_send
            None,                        // Don't set later timestamps to avoid underflow
            None,
            None,
            None,
            None,
        );
        stats.add_sample(profiling);
    }

    let source_stats = stats.get_source_to_query_stats();

    // Verify all fields are populated
    assert_eq!(source_stats.count, 10);
    assert!(source_stats.mean > 0.0);
    assert!(source_stats.variance >= 0.0); // Allow 0 variance if all same
    assert!(source_stats.std_dev >= 0.0);
    assert!(source_stats.min > 0.0);
    assert!(source_stats.max > 0.0);
    assert!(source_stats.p50 > 0.0);
    assert!(source_stats.p95 > 0.0);
    assert!(source_stats.p99 > 0.0);

    // Verify ordering: min <= p50 <= p95 <= p99 <= max
    assert!(source_stats.min <= source_stats.p50);
    assert!(source_stats.p50 <= source_stats.p95);
    assert!(source_stats.p95 <= source_stats.p99);
    assert!(source_stats.p99 <= source_stats.max);
}

#[test]
fn test_format_stats_output() {
    let stats = MetricStats {
        count: 1000,
        mean: 1500000.0, // 1.5ms in nanoseconds
        variance: 250000.0,
        std_dev: 500.0,
        min: 1000000.0, // 1ms
        max: 2000000.0, // 2ms
        p50: 1500000.0, // 1.5ms
        p95: 1900000.0, // 1.9ms
        p99: 1990000.0, // 1.99ms
    };

    let formatted = ProfilerReaction::format_stats("Test Metric", &stats);

    // Verify the format includes all key information
    assert!(formatted.contains("Test Metric:"));
    assert!(formatted.contains("mean=1.50ms"));
    assert!(formatted.contains("stddev=0.00ms"));
    assert!(formatted.contains("min=1.00ms"));
    assert!(formatted.contains("max=2.00ms"));
    assert!(formatted.contains("p50=1.50ms"));
    assert!(formatted.contains("p95=1.90ms"));
    assert!(formatted.contains("p99=1.99ms"));
    assert!(formatted.contains("(n=1000)"));
}

#[test]
fn test_large_sample_count() {
    let mut stats = ProfilingStats::new(10000);

    // Add 10000 samples
    for i in 0..10000 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(i),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    assert_eq!(stats.count, 10000);
    assert_eq!(stats.samples.len(), 10000);

    let source_stats = stats.get_source_to_query_stats();

    // With 10000 samples from 0 to 9999, mean should be ~4999.5
    assert!((source_stats.mean - 4999.5).abs() < 1.0);

    // Verify percentiles are calculated correctly
    assert!(source_stats.p50 >= 4500.0 && source_stats.p50 <= 5500.0);
    assert!(source_stats.p95 >= 9400.0 && source_stats.p95 <= 9600.0);
    assert!(source_stats.p99 >= 9800.0 && source_stats.p99 <= 9999.0);
}

#[test]
fn test_variance_with_uniform_distribution() {
    let mut stats = ProfilingStats::new(1000);

    // Add samples with uniform distribution: all same value
    for _ in 0..100 {
        let profiling = create_profiling_metadata(
            Some(0),
            Some(5000),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        stats.add_sample(profiling);
    }

    let source_stats = stats.get_source_to_query_stats();

    // With all samples the same, variance should be 0
    assert_eq!(source_stats.variance, 0.0);
    assert_eq!(source_stats.std_dev, 0.0);
    assert_eq!(source_stats.mean, 5000.0);
    assert_eq!(source_stats.min, 5000.0);
    assert_eq!(source_stats.max, 5000.0);
    assert_eq!(source_stats.p50, 5000.0);
    assert_eq!(source_stats.p95, 5000.0);
    assert_eq!(source_stats.p99, 5000.0);
}
