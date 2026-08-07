# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-03

### Features

- Initial shared PostgreSQL type conversion crate for source and bootstrap plugins
- `PostgresValue::UnchangedToast` for pgoutput unchanged-TOAST placeholders
- Canonical `PostgresValue::to_element_value` mapping (temporals, numeric→Float, bytea base64, Null present)
- Text OID decoder for pgoutput/bootstrap parity ([#669](https://github.com/drasi-project/drasi-core/issues/669), [#670](https://github.com/drasi-project/drasi-core/issues/670), [#672](https://github.com/drasi-project/drasi-core/issues/672))
