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

//! Shared MySQL value formatting helpers used by bootstrap and CDC paths.
//!
//! Canonical rules for bootstrap ↔ CDC parity:
//! - `ENUM` → member label string
//! - `SET` → comma-separated member labels in declaration order
//! - `TIMESTAMP` → UTC `YYYY-MM-DD HH:MM:SS[.ffffff]`
//! - fractional `DATETIME`/`TIME` → preserve sub-second digits
//! - `JSON` → compact `serde_json` serialization

use chrono::{TimeZone, Utc};

/// Format a MySQL DATETIME/DATE value.
///
/// When `fsp` is `Some(n)`, always emit exactly `n` fractional digits (padded/truncated).
/// When `fsp` is `None`, emit 6 fractional digits only if `micros != 0`.
#[allow(clippy::too_many_arguments)]
pub fn format_datetime(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
    fsp: Option<u8>,
) -> String {
    let base = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    append_fractional(base, micros, fsp)
}

/// Format a MySQL TIME value as `HHH:MM:SS[.ffffff]` with hours including day overflow.
#[allow(clippy::too_many_arguments)]
pub fn format_time(
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
    fsp: Option<u8>,
) -> String {
    let total_hours = days * 24 + u32::from(hours);
    let sign = if negative { "-" } else { "" };
    let base = format!("{sign}{total_hours:03}:{minutes:02}:{seconds:02}");
    append_fractional(base, micros, fsp)
}

/// Format a Unix-epoch TIMESTAMP (seconds + optional micros) as a UTC datetime string.
pub fn format_timestamp_epoch(secs: i64, micros: u32, fsp: Option<u8>) -> String {
    match Utc.timestamp_opt(secs, micros.saturating_mul(1_000)) {
        chrono::LocalResult::Single(dt) => {
            let base = dt.format("%Y-%m-%d %H:%M:%S").to_string();
            append_fractional(base, micros, fsp)
        }
        _ => {
            // Fall back to a best-effort string if the epoch is out of range.
            let base = format!("{secs}");
            append_fractional(base, micros, fsp)
        }
    }
}

/// Parse a TIMESTAMP2 binlog text payload (`"secs"` or `"secs.micros"`) into components.
pub fn parse_timestamp_epoch_text(text: &str) -> Option<(i64, u32)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((secs_part, frac_part)) = text.split_once('.') {
        let secs = secs_part.parse::<i64>().ok()?;
        let micros = parse_fractional_to_micros(frac_part)?;
        Some((secs, micros))
    } else {
        let secs = text.parse::<i64>().ok()?;
        Some((secs, 0))
    }
}

/// Map a 1-based ENUM ordinal to its label. Unknown ordinals stringify the ordinal.
pub fn enum_label(ordinal: u64, labels: &[String]) -> String {
    if ordinal == 0 {
        // MySQL uses 0 for the empty/invalid enum error value.
        return String::new();
    }
    labels
        .get((ordinal as usize).saturating_sub(1))
        .cloned()
        .unwrap_or_else(|| ordinal.to_string())
}

/// Decode a little-endian SET bitmask into a comma-separated label list.
///
/// Member `i` corresponds to bit `i` (0-based). Labels appear in declaration order.
pub fn set_labels(bitmask: &[u8], labels: &[String]) -> String {
    let mut selected = Vec::new();
    for (idx, label) in labels.iter().enumerate() {
        let byte_idx = idx / 8;
        let bit_idx = idx % 8;
        if let Some(byte) = bitmask.get(byte_idx) {
            if (byte >> bit_idx) & 1 == 1 {
                selected.push(label.as_str());
            }
        }
    }
    selected.join(",")
}

/// Canonicalize JSON text to compact `serde_json` form. Returns original text if parse fails.
pub fn canonicalize_json_text(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    }
}

fn append_fractional(base: String, micros: u32, fsp: Option<u8>) -> String {
    match fsp {
        Some(0) | None if micros == 0 => base,
        Some(0) => base, // fsp explicitly 0: drop any residual micros
        Some(n) => {
            let n = n.min(6) as usize;
            let scaled = micros_to_fsp_digits(micros, n);
            format!("{base}.{scaled:0n$}")
        }
        None => format!("{base}.{micros:06}"),
    }
}

fn micros_to_fsp_digits(micros: u32, fsp: usize) -> u32 {
    // micros is always 0..1_000_000. Scale down to fsp digits.
    let divisor = 10u32.pow((6 - fsp) as u32);
    micros / divisor
}

fn parse_fractional_to_micros(frac: &str) -> Option<u32> {
    if frac.is_empty() || frac.len() > 6 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut padded = frac.to_string();
    while padded.len() < 6 {
        padded.push('0');
    }
    padded.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_datetime_no_fraction() {
        assert_eq!(
            format_datetime(2025, 6, 15, 13, 45, 30, 0, None),
            "2025-06-15 13:45:30"
        );
    }

    #[test]
    fn test_format_datetime_with_micros() {
        assert_eq!(
            format_datetime(2025, 6, 15, 13, 45, 30, 123456, None),
            "2025-06-15 13:45:30.123456"
        );
    }

    #[test]
    fn test_format_datetime_with_fsp() {
        assert_eq!(
            format_datetime(2025, 6, 15, 13, 45, 30, 123456, Some(3)),
            "2025-06-15 13:45:30.123"
        );
        assert_eq!(
            format_datetime(2025, 6, 15, 13, 45, 30, 0, Some(6)),
            "2025-06-15 13:45:30.000000"
        );
    }

    #[test]
    fn test_format_time() {
        assert_eq!(
            format_time(false, 1, 13, 45, 30, 500, None),
            "037:45:30.000500"
        );
        assert_eq!(format_time(true, 0, 1, 2, 3, 0, Some(0)), "-001:02:03");
    }

    #[test]
    fn test_format_timestamp_epoch() {
        // 2025-06-15 13:45:30 UTC
        let secs = 1_749_995_130i64;
        assert_eq!(format_timestamp_epoch(secs, 0, None), "2025-06-15 13:45:30");
        assert_eq!(
            format_timestamp_epoch(secs, 123456, Some(6)),
            "2025-06-15 13:45:30.123456"
        );
    }

    #[test]
    fn test_parse_timestamp_epoch_text() {
        assert_eq!(
            parse_timestamp_epoch_text("1749995130"),
            Some((1749995130, 0))
        );
        assert_eq!(
            parse_timestamp_epoch_text("1749995130.123456"),
            Some((1749995130, 123456))
        );
        assert_eq!(
            parse_timestamp_epoch_text("1749995130.12"),
            Some((1749995130, 120000))
        );
        assert_eq!(parse_timestamp_epoch_text("not-a-number"), None);
    }

    #[test]
    fn test_enum_label() {
        let labels = vec!["red".into(), "green".into(), "blue".into()];
        assert_eq!(enum_label(0, &labels), "");
        assert_eq!(enum_label(1, &labels), "red");
        assert_eq!(enum_label(2, &labels), "green");
        assert_eq!(enum_label(3, &labels), "blue");
        assert_eq!(enum_label(9, &labels), "9");
    }

    #[test]
    fn test_set_labels() {
        let labels = vec!["a".into(), "b".into(), "c".into()];
        // 0b101 = a + c
        assert_eq!(set_labels(&[0b101], &labels), "a,c");
        assert_eq!(set_labels(&[0], &labels), "");
        assert_eq!(set_labels(&[0b111], &labels), "a,b,c");
        // multi-byte: member index 8 (9th) set
        let many: Vec<String> = (0..10).map(|i| format!("m{i}")).collect();
        assert_eq!(set_labels(&[0x00, 0x01], &many), "m8");
    }

    #[test]
    fn test_canonicalize_json_text() {
        assert_eq!(canonicalize_json_text(r#"{"k": 1}"#), r#"{"k":1}"#);
        assert_eq!(canonicalize_json_text(r#"[ 1, 2 ]"#), "[1,2]");
        // invalid JSON is left alone
        assert_eq!(canonicalize_json_text("not-json"), "not-json");
    }
}
