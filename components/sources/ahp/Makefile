build:
	cargo build -p drasi-source-ahp

test:
	cargo test -p drasi-source-ahp

integration-test:
	cargo test -p drasi-source-ahp --test integration_tests -- --nocapture

lint:
	cargo clippy -p drasi-source-ahp --all-targets -- -D warnings
