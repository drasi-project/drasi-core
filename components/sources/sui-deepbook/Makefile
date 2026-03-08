build:
	cargo build -p drasi-source-sui-deepbook

test:
	cargo test -p drasi-source-sui-deepbook

integration-test:
	cargo test -p drasi-source-sui-deepbook --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-source-sui-deepbook --all-targets -- -D warnings
