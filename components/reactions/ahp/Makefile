.PHONY: build test lint example

build:
	cargo build -p drasi-reaction-ahp

test:
	cargo test -p drasi-reaction-ahp

lint:
	cargo clippy -p drasi-reaction-ahp --all-targets --features dynamic-plugin,mock-host -- -D warnings

example:
	cd ../../../examples/lib/ahp-incident-agents && cargo run
