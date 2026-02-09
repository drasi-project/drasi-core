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

use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current timestamp in milliseconds since Unix epoch.
///
/// Uses chrono's [`Utc::now()`](chrono::Utc::now) which handles time zones correctly.
///
/// # Returns
///
/// The timestamp in milliseconds as `u64`, or an error if the system clock
/// produces a negative timestamp (set to before Unix epoch).
pub fn get_current_timestamp_millis() -> Result<u64> {
    let millis = chrono::Utc::now().timestamp_millis();
    if millis < 0 {
        anyhow::bail!("System time produced negative timestamp: {millis} ms");
    }
    Ok(millis as u64)
}

/// Returns the current timestamp in nanoseconds since Unix epoch.
///
/// Uses chrono's [`Utc::now()`](chrono::Utc::now) with automatic fallback to
/// millisecond precision if nanoseconds would overflow (dates outside 1677-2262).
///
/// # Returns
///
/// The timestamp in nanoseconds as `u64`, or an error if the system clock
/// produces a negative timestamp.
#[allow(dead_code)]
pub fn get_current_timestamp_nanos() -> Result<u64> {
    // Try to get nanosecond precision first
    match chrono::Utc::now().timestamp_nanos_opt() {
        Some(nanos) => {
            // Ensure it's not negative (shouldn't happen with Utc::now())
            if nanos < 0 {
                anyhow::bail!("System time produced negative timestamp: {nanos}");
            }
            Ok(nanos as u64)
        }
        None => {
            // Fallback to millisecond precision and convert to nanos
            // This handles dates outside the nanosecond range (1677-2262)
            log::warn!("Timestamp overflow detected, falling back to millisecond precision");
            let millis = chrono::Utc::now().timestamp_millis();
            if millis < 0 {
                anyhow::bail!("System time produced negative timestamp: {millis} ms");
            }
            // Convert milliseconds to nanoseconds
            Ok((millis as u64) * 1_000_000)
        }
    }
}

/// Returns the current system time as milliseconds since Unix epoch.
///
/// Uses [`SystemTime`] directly rather than chrono for minimal overhead.
///
/// # Returns
///
/// The duration in milliseconds as `u64`, or an error if the system clock
/// is set to before the Unix epoch (January 1, 1970).
///
/// # Note
///
/// The conversion from `u128` to `u64` is safe for all practical timestamps.
/// Overflow would only occur ~584 million years after the Unix epoch.
pub fn get_system_time_millis() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System time is before Unix epoch (January 1, 1970)")
        .map(|duration| duration.as_millis() as u64)
}

/// Returns the current system time as nanoseconds since Unix epoch.
///
/// Uses [`SystemTime`] directly rather than chrono for minimal overhead.
///
/// # Returns
///
/// The duration in nanoseconds as `u64`, or an error if the system clock
/// is set to before the Unix epoch (January 1, 1970).
///
/// # Note
///
/// The conversion from `u128` to `u64` is safe until approximately year 2554.
/// After that, nanosecond precision would overflow `u64`. This is unlikely
/// to be a practical concern.
#[allow(dead_code)]
pub fn get_system_time_nanos() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System time is before Unix epoch (January 1, 1970)")
        .map(|duration| duration.as_nanos() as u64)
}

/// Returns a timestamp using multiple fallback strategies.
///
/// Tries the following methods in order:
/// 1. Chrono millisecond precision (handles time zones correctly)
/// 2. [`SystemTime`] (if chrono fails)
/// 3. Provided default value (if all methods fail)
///
/// # Arguments
///
/// * `default_on_error` - Optional fallback value if all timestamp methods fail
///
/// # Returns
///
/// A timestamp in milliseconds using the first successful method.
///
/// # Errors
///
/// Returns an error only if all methods fail AND no default is provided.
#[allow(dead_code)]
pub fn get_timestamp_with_fallback(default_on_error: Option<u64>) -> Result<u64> {
    // Try chrono first (handles time zones correctly)
    if let Ok(timestamp) = get_current_timestamp_millis() {
        return Ok(timestamp);
    }

    // Try SystemTime as fallback
    if let Ok(timestamp) = get_system_time_millis() {
        log::debug!("Using SystemTime fallback for timestamp");
        return Ok(timestamp);
    }

    // Use default if provided
    if let Some(default) = default_on_error {
        log::error!("All timestamp methods failed, using default value: {default}");
        return Ok(default);
    }

    anyhow::bail!("Unable to obtain valid timestamp from system")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_current_timestamp_millis() {
        // Should succeed for current time
        let result = get_current_timestamp_millis();
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp > 0);
    }

    #[test]
    fn test_get_system_time_millis() {
        // Should succeed for current time
        let result = get_system_time_millis();
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp > 0);
    }

    #[test]
    fn test_get_current_timestamp_nanos() {
        // Should succeed for current time
        let result = get_current_timestamp_nanos();
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp > 0);
    }

    #[test]
    fn test_get_system_time_nanos() {
        // Should succeed for current time
        let result = get_system_time_nanos();
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp > 0);
    }

    #[test]
    fn test_get_timestamp_with_fallback() {
        // Should succeed without needing fallback
        let result = get_timestamp_with_fallback(None);
        assert!(result.is_ok());

        // Should use default if provided (in error scenarios)
        let result_with_default = get_timestamp_with_fallback(Some(42));
        assert!(result_with_default.is_ok());
    }
}
