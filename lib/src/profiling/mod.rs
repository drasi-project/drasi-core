// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

pub use drasi_core_types::profiling::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiling_metadata_new() {
        let metadata = ProfilingMetadata::new();
        assert!(metadata.source_receive_ns.is_some());
        assert!(metadata.source_ns.is_none());
    }

    #[test]
    fn test_profiling_metadata_with_source_timestamp() {
        let source_ts = 1000000000;
        let metadata = ProfilingMetadata::with_source_timestamp(source_ts);
        assert_eq!(metadata.source_ns, Some(source_ts));
        assert!(metadata.source_receive_ns.is_some());
    }

    #[test]
    fn test_elapsed_calculations() {
        // Set up timestamps for testing
        let metadata = ProfilingMetadata {
            source_receive_ns: Some(1000),
            source_send_ns: Some(2000),
            query_receive_ns: Some(3000),
            query_core_call_ns: Some(3500),
            query_core_return_ns: Some(5500),
            query_send_ns: Some(6000),
            reaction_receive_ns: Some(7000),
            reaction_complete_ns: Some(8000),
            ..Default::default()
        };

        // Test individual elapsed time calculations
        assert_eq!(metadata.elapsed_source_internal(), Some(1000));
        assert_eq!(metadata.elapsed_source_to_query(), Some(1000));
        assert_eq!(metadata.elapsed_query_processing(), Some(2000));
        assert_eq!(metadata.elapsed_query_internal(), Some(3000));
        assert_eq!(metadata.elapsed_query_to_reaction(), Some(1000));
        assert_eq!(metadata.elapsed_reaction_processing(), Some(1000));
        assert_eq!(metadata.elapsed_total(), Some(7000));
    }

    #[test]
    fn test_elapsed_with_missing_timestamps() {
        let metadata = ProfilingMetadata::default();

        assert_eq!(metadata.elapsed_source_to_query(), None);
        assert_eq!(metadata.elapsed_query_processing(), None);
        assert_eq!(metadata.elapsed_query_to_reaction(), None);
        assert_eq!(metadata.elapsed_reaction_processing(), None);
        assert_eq!(metadata.elapsed_total(), None);
    }

    #[test]
    fn test_elapsed_summary_ms() {
        let metadata = ProfilingMetadata {
            source_receive_ns: Some(1_000_000),    // 1ms
            source_send_ns: Some(2_000_000),       // 2ms
            query_receive_ns: Some(3_000_000),     // 3ms
            query_core_call_ns: Some(3_500_000),   // 3.5ms
            query_core_return_ns: Some(5_500_000), // 5.5ms
            query_send_ns: Some(6_000_000),        // 6ms
            reaction_receive_ns: Some(7_000_000),  // 7ms
            reaction_complete_ns: Some(8_000_000), // 8ms
            ..Default::default()
        };

        let summary = metadata.elapsed_summary_ms();

        assert_eq!(summary.source_internal_ms, Some(1.0));
        assert_eq!(summary.source_to_query_ms, Some(1.0));
        assert_eq!(summary.query_processing_ms, Some(2.0));
        assert_eq!(summary.query_internal_ms, Some(3.0));
        assert_eq!(summary.query_to_reaction_ms, Some(1.0));
        assert_eq!(summary.reaction_processing_ms, Some(1.0));
        assert_eq!(summary.total_ms, Some(7.0));
    }

    #[test]
    fn test_is_enabled() {
        let mut metadata = ProfilingMetadata::default();
        assert!(!metadata.is_enabled());

        metadata.source_receive_ns = Some(1000);
        assert!(metadata.is_enabled());
    }

    #[test]
    fn test_merge_metadata() {
        let mut metadata1 = ProfilingMetadata {
            source_receive_ns: Some(1000),
            source_send_ns: Some(2000),
            ..Default::default()
        };

        let metadata2 = ProfilingMetadata {
            query_receive_ns: Some(3000),
            query_send_ns: Some(4000),
            source_send_ns: Some(9999), // Should not override
            ..Default::default()
        };

        metadata1.merge(&metadata2);

        assert_eq!(metadata1.source_receive_ns, Some(1000));
        assert_eq!(metadata1.source_send_ns, Some(2000)); // Not overridden
        assert_eq!(metadata1.query_receive_ns, Some(3000)); // Merged
        assert_eq!(metadata1.query_send_ns, Some(4000)); // Merged
    }

    #[test]
    fn test_profiling_config_default() {
        let config = ProfilingConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.sampling_rate, 1.0);
        assert!(config.include_bootstrap);
    }

    #[test]
    fn test_profiling_config_sampling_rate() {
        let config = ProfilingConfig::enabled()
            .with_sampling_rate(0.5)
            .with_include_bootstrap(false);

        assert!(config.enabled);
        assert_eq!(config.sampling_rate, 0.5);
        assert!(!config.include_bootstrap);

        // Test clamping
        let config = ProfilingConfig::enabled().with_sampling_rate(1.5);
        assert_eq!(config.sampling_rate, 1.0);

        let config = ProfilingConfig::enabled().with_sampling_rate(-0.5);
        assert_eq!(config.sampling_rate, 0.0);
    }

    #[test]
    fn test_format_duration_ns() {
        assert_eq!(format_duration_ns(500), "500ns");
        assert_eq!(format_duration_ns(1_500), "1.50µs");
        assert_eq!(format_duration_ns(1_500_000), "1.50ms");
        assert_eq!(format_duration_ns(1_500_000_000), "1.50s");
    }

    #[test]
    fn test_ns_conversions() {
        assert_eq!(ns_to_ms(1_000_000), 1.0);
        assert_eq!(ns_to_ms(1_500_000), 1.5);
        assert_eq!(ns_to_secs(1_000_000_000), 1.0);
        assert_eq!(ns_to_secs(1_500_000_000), 1.5);
    }

    #[test]
    fn test_timestamp_ns() {
        let ts1 = timestamp_ns();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let ts2 = timestamp_ns();
        assert!(ts2 > ts1);
    }

    #[test]
    fn test_should_profile() {
        let config = ProfilingConfig {
            enabled: false,
            sampling_rate: 1.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        };
        assert!(!config.should_profile());

        let config = ProfilingConfig {
            enabled: true,
            sampling_rate: 0.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        };
        assert!(!config.should_profile());

        let config = ProfilingConfig {
            enabled: true,
            sampling_rate: 1.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        };
        assert!(config.should_profile());
    }
}
