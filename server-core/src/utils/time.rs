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

/// Get the current timestamp in nanoseconds since Unix epoch.
///
/// This function handles edge cases where nanosecond precision would overflow,
/// falling back to millisecond precision when necessary.
///
/// # Returns
///
/// Returns the timestamp in nanoseconds, or an error if the system time is invalid.
///
/// # Examples
///
/// ```
/// use drasi_server_core::utils::time::get_current_timestamp_nanos;
///
/// let timestamp = get_current_timestamp_nanos().expect("Failed to get timestamp");
/// ```
pub fn get_current_timestamp_nanos() -> Result<u64> {
    // Try to get nanosecond precision first
    match chrono::Utc::now().timestamp_nanos_opt() {
        Some(nanos) => {
            // Ensure it's not negative (shouldn't happen with Utc::now())
            if nanos < 0 {
                anyhow::bail!("System time produced negative timestamp: {}", nanos);
            }
            Ok(nanos as u64)
        }
        None => {
            // Fallback to millisecond precision and convert to nanos
            // This handles dates outside the nanosecond range (1677-2262)
            log::warn!("Timestamp overflow detected, falling back to millisecond precision");
            let millis = chrono::Utc::now().timestamp_millis();
            if millis < 0 {
                anyhow::bail!("System time produced negative timestamp: {} ms", millis);
            }
            // Convert milliseconds to nanoseconds
            Ok((millis as u64) * 1_000_000)
        }
    }
}

/// Get the current SystemTime duration since Unix epoch in nanoseconds.
///
/// This function properly handles the case where system time is before Unix epoch.
///
/// # Returns
///
/// Returns the duration in nanoseconds, or an error if system time is before Unix epoch.
///
/// # Examples
///
/// ```
/// use drasi_server_core::utils::time::get_system_time_nanos;
///
/// let timestamp = get_system_time_nanos().expect("Failed to get system time");
/// ```
pub fn get_system_time_nanos() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System time is before Unix epoch (January 1, 1970)")
        .map(|duration| duration.as_nanos() as u64)
}

/// Get the current timestamp with automatic fallback strategy.
///
/// This function tries multiple methods to get a valid timestamp:
/// 1. Chrono nanosecond precision
/// 2. Chrono millisecond precision (if nanos overflow)
/// 3. SystemTime (if chrono fails)
/// 4. Default value (if all else fails and default is provided)
///
/// # Arguments
///
/// * `default_on_error` - Optional default value to use if all timestamp methods fail
///
/// # Returns
///
/// Returns a timestamp in nanoseconds, using the first successful method.
///
/// # Examples
///
/// ```
/// use drasi_server_core::utils::time::get_timestamp_with_fallback;
///
/// // Will use current time or 0 if all methods fail
/// let timestamp = get_timestamp_with_fallback(Some(0));
/// ```
pub fn get_timestamp_with_fallback(default_on_error: Option<u64>) -> Result<u64> {
    // Try chrono first (handles time zones correctly)
    if let Ok(timestamp) = get_current_timestamp_nanos() {
        return Ok(timestamp);
    }

    // Try SystemTime as fallback
    if let Ok(timestamp) = get_system_time_nanos() {
        log::debug!("Using SystemTime fallback for timestamp");
        return Ok(timestamp);
    }

    // Use default if provided
    if let Some(default) = default_on_error {
        log::error!(
            "All timestamp methods failed, using default value: {}",
            default
        );
        return Ok(default);
    }

    anyhow::bail!("Unable to obtain valid timestamp from system")
}

#[cfg(test)]
mod tests {
    use super::*;

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
