.PHONY: build test integration-test lint schema update-schema

build:
	cargo build -p drasi-reaction-http

test:
	cargo test -p drasi-reaction-http

integration-test:
	cargo test -p drasi-reaction-http --test integration_tests -- --nocapture

lint:
	cargo clippy -p drasi-reaction-http --all-targets -- -D warnings

# Verify schema/output.schema.json is in sync with src/output.rs.
# Fails (with a regenerate hint) if the Rust types have drifted from the
# committed schema.
schema:
	cargo test -p drasi-reaction-http --test output_schema

# Regenerate schema/output.schema.json from src/output.rs. Run this any
# time DefaultChangeNotification / Operation / BatchEnvelope changes.
update-schema:
	DRASI_UPDATE_SCHEMA=1 cargo test -p drasi-reaction-http --test output_schema -- --nocapture
