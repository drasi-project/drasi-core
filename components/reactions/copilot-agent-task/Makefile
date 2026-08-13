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

# Verify schema/*.schema.json is in sync with src/prompt.rs.
schema:
	cargo test -p drasi-reaction-copilot-agent-task --test output_schema

# Regenerate schema/*.schema.json from src/prompt.rs. Run this any time
# WorkGraphEvent/v1 or workgraph.execution/v1 changes.
update-schema:
	DRASI_UPDATE_SCHEMA=1 cargo test -p drasi-reaction-copilot-agent-task --test output_schema -- --nocapture

