.PHONY: build test integration-test lint schema update-schema

build:
	cargo build -p drasi-reaction-copilot-agent-task

test:
	cargo test -p drasi-reaction-copilot-agent-task

integration-test:
	cargo test -p drasi-reaction-copilot-agent-task --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-copilot-agent-task --all-targets -- -D warnings
	cargo fmt -p drasi-reaction-copilot-agent-task -- --check

# Verify schema/workgraph-execution-state-v1.schema.json is in sync with
# src/state.rs (the durable execution-state telemetry schema).
schema:
	cargo test -p drasi-reaction-copilot-agent-task --test output_schema

# Regenerate schema/workgraph-execution-state-v1.schema.json from src/state.rs.
# Run this any time the WorkGraphExecutionStateV1 log envelope changes.
update-schema:
	DRASI_UPDATE_SCHEMA=1 cargo test -p drasi-reaction-copilot-agent-task --test output_schema -- --nocapture

