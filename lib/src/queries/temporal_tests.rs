// Copyright 2026 The Drasi Authors.
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

use crate::test_helpers::native_query_value as convert_variable_value_to_json;
use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDate, NaiveTime, TimeZone};
use drasi_core::evaluation::variable_value::{
    duration::Duration as VarDuration, zoned_datetime::ZonedDateTime as VarZonedDateTime,
    zoned_time::ZonedTime as VarZonedTime, VariableValue,
};

#[test]
fn temporal_values_serialize_as_plain_strings() {
    let date = NaiveDate::from_ymd_opt(2024, 6, 15).expect("valid date");
    let local_time = NaiveTime::from_hms_micro_opt(10, 30, 45, 123_456).expect("valid time");
    let offset = FixedOffset::east_opt(3600).expect("valid fixed offset");
    let zoned_time = VarZonedTime::new(local_time, offset);
    let local_datetime = date
        .and_hms_micro_opt(10, 30, 45, 123_456)
        .expect("valid local datetime");
    let zoned_datetime = VarZonedDateTime::new(
        offset
            .with_ymd_and_hms(2024, 6, 15, 10, 30, 45)
            .single()
            .expect("valid zoned datetime"),
        Some("Europe/Berlin".to_string()),
    );
    let duration = VarDuration::new(ChronoDuration::seconds(90), 0, 0);

    for (value, expected) in [
        (VariableValue::Date(date), date.to_string()),
        (VariableValue::LocalTime(local_time), local_time.to_string()),
        (VariableValue::ZonedTime(zoned_time), zoned_time.to_string()),
        (
            VariableValue::LocalDateTime(local_datetime),
            local_datetime.to_string(),
        ),
        (
            VariableValue::ZonedDateTime(zoned_datetime.clone()),
            zoned_datetime.datetime().to_rfc3339(),
        ),
        (
            VariableValue::Duration(duration.clone()),
            duration.to_string(),
        ),
    ] {
        assert_eq!(
            convert_variable_value_to_json(&value),
            serde_json::Value::String(expected)
        );
    }
}
