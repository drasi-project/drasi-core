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

//! Shared serde/OpenAPI DTOs for MySQL plugin descriptors.
//!
//! Gated behind the `api` feature so that only the plugin descriptor crates
//! (which already depend on `utoipa`) pull in the schema machinery.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::SslMode;

/// SSL mode DTO (mirrors [`SslMode`]).
///
/// Single source of truth for the `ssl_mode` config field shared by the MySQL
/// source and bootstrap plugin descriptors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = mysql::SslMode)]
#[serde(rename_all = "snake_case")]
pub enum SslModeDto {
    Disabled,
    #[default]
    IfAvailable,
    Require,
    RequireVerifyCa,
    RequireVerifyFull,
}

impl FromStr for SslModeDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "disabled" => Ok(SslModeDto::Disabled),
            "if_available" | "ifavailable" => Ok(SslModeDto::IfAvailable),
            "require" => Ok(SslModeDto::Require),
            "require_verify_ca" | "requireverifyca" => Ok(SslModeDto::RequireVerifyCa),
            "require_verify_full" | "requireverifyfull" => Ok(SslModeDto::RequireVerifyFull),
            _ => Err(format!("Invalid SSL mode: {s}")),
        }
    }
}

impl From<SslModeDto> for SslMode {
    fn from(dto: SslModeDto) -> Self {
        match dto {
            SslModeDto::Disabled => SslMode::Disabled,
            SslModeDto::IfAvailable => SslMode::IfAvailable,
            SslModeDto::Require => SslMode::Require,
            SslModeDto::RequireVerifyCa => SslMode::RequireVerifyCa,
            SslModeDto::RequireVerifyFull => SslMode::RequireVerifyFull,
        }
    }
}

impl From<&SslMode> for SslModeDto {
    fn from(mode: &SslMode) -> Self {
        match mode {
            SslMode::Disabled => SslModeDto::Disabled,
            SslMode::IfAvailable => SslModeDto::IfAvailable,
            SslMode::Require => SslModeDto::Require,
            SslMode::RequireVerifyCa => SslModeDto::RequireVerifyCa,
            SslMode::RequireVerifyFull => SslModeDto::RequireVerifyFull,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_parses_canonical_and_alias_forms() {
        assert_eq!("disabled".parse(), Ok(SslModeDto::Disabled));
        assert_eq!("if_available".parse(), Ok(SslModeDto::IfAvailable));
        assert_eq!("ifavailable".parse(), Ok(SslModeDto::IfAvailable));
        assert_eq!("require".parse(), Ok(SslModeDto::Require));
        assert_eq!("require_verify_ca".parse(), Ok(SslModeDto::RequireVerifyCa));
        assert_eq!("requireverifyca".parse(), Ok(SslModeDto::RequireVerifyCa));
        assert_eq!(
            "require_verify_full".parse(),
            Ok(SslModeDto::RequireVerifyFull)
        );
        assert_eq!(
            "requireverifyfull".parse(),
            Ok(SslModeDto::RequireVerifyFull)
        );
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!("IfAvailable".parse(), Ok(SslModeDto::IfAvailable));
        assert_eq!("REQUIRE".parse(), Ok(SslModeDto::Require));
    }

    #[test]
    fn from_str_rejects_unknown_values() {
        assert!("bogus".parse::<SslModeDto>().is_err());
        assert!("".parse::<SslModeDto>().is_err());
    }

    #[test]
    fn default_matches_ssl_mode_default() {
        assert_eq!(SslModeDto::default(), SslModeDto::IfAvailable);
        assert_eq!(SslMode::from(SslModeDto::default()), SslMode::default());
    }

    #[test]
    fn converts_between_dto_and_domain_type() {
        for (dto, mode) in [
            (SslModeDto::Disabled, SslMode::Disabled),
            (SslModeDto::IfAvailable, SslMode::IfAvailable),
            (SslModeDto::Require, SslMode::Require),
            (SslModeDto::RequireVerifyCa, SslMode::RequireVerifyCa),
            (SslModeDto::RequireVerifyFull, SslMode::RequireVerifyFull),
        ] {
            assert_eq!(SslMode::from(dto), mode);
            assert_eq!(SslModeDto::from(&mode), dto);
        }
    }
}
