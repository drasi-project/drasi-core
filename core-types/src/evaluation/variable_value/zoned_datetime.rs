// Copyright 2024 The Drasi Authors.
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

use crate::evaluation::{
    temporal_constants::{self, UTC_FIXED_OFFSET},
    EvaluationError,
};
use chrono::{DateTime, FixedOffset, TimeZone};
use chrono_tz::Tz;
use core::fmt::{self, Display};
use serde::{Deserialize, Serialize};
use std::hash::Hash;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ZonedDateTime {
    datetime: DateTime<FixedOffset>,
    timezone_name: Option<String>, // timezone name is optional; depending on the input
}

impl ZonedDateTime {
    pub fn new(datetime: DateTime<FixedOffset>, timezone_name: Option<String>) -> Self {
        ZonedDateTime {
            datetime,
            timezone_name,
        }
    }

    pub fn from_epoch_millis(epoch_millis: i64) -> Self {
        let offset = *UTC_FIXED_OFFSET;
        let datetime = offset.timestamp_millis_opt(epoch_millis).unwrap();
        ZonedDateTime {
            datetime,
            timezone_name: None,
        }
    }

    pub fn from_string(input: &str) -> Result<Self, EvaluationError> {
        if let Some(bracket_pos) = input.find('[') {
            let base_str = &input[..bracket_pos];
            let end_bracket = match input.find(']') {
                Some(pos) => pos,
                None => {
                    return Err(EvaluationError::FormatError {
                        expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                            .to_string(),
                    })
                }
            };
            let tz_str = &input[bracket_pos + 1..end_bracket];
            let tz = match extract_iana_timezone(tz_str) {
                Some(tz) => tz,
                None => {
                    return Err(EvaluationError::FormatError {
                        expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                            .to_string(),
                    })
                }
            };

            let base_dt = match time::PrimitiveDateTime::parse(
                base_str,
                &time::format_description::well_known::Iso8601::DEFAULT,
            ) {
                Ok(dt) => {
                    DateTime::from_timestamp_nanos(dt.assume_utc().unix_timestamp_nanos() as i64)
                        .naive_utc()
                }
                Err(e) => {
                    return Err(EvaluationError::FormatError {
                        expected: e.to_string(),
                    })
                }
            };

            let dt2 = match tz.from_local_datetime(&base_dt) {
                chrono::offset::LocalResult::Single(d) => d,
                chrono::offset::LocalResult::Ambiguous(d1, _) => d1,
                chrono::offset::LocalResult::None => {
                    return Err(EvaluationError::FormatError {
                        expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                            .to_string(),
                    })
                }
            };

            return Ok(ZonedDateTime::new(
                dt2.fixed_offset(),
                Some(tz.name().to_string()),
            ));
        }

        match time::OffsetDateTime::parse(
            input,
            &time::format_description::well_known::Iso8601::DEFAULT,
        ) {
            Ok(dt) => Ok(Self::from_epoch_millis(
                (dt.unix_timestamp_nanos() / 1_000_000) as i64,
            )),
            Err(e) => Err(EvaluationError::FormatError {
                expected: e.to_string(),
            }),
        }
    }

    pub fn datetime(&self) -> &DateTime<FixedOffset> {
        &self.datetime
    }

    pub fn timezone_name(&self) -> &Option<String> {
        &self.timezone_name
    }
}

impl Display for ZonedDateTime {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.datetime.fmt(formatter)
    }
}

fn extract_iana_timezone(input: &str) -> Option<Tz> {
    let timezone_str = input.replace(' ', "_").replace(['[', ']'], "");
    if timezone_str == "Z" || timezone_str.contains('+') || timezone_str.contains('-') {
        return Some(Tz::UTC);
    }
    let tz: Tz = match timezone_str.parse() {
        Ok(tz) => tz,
        Err(_) => return None,
    };
    Some(tz)
}
