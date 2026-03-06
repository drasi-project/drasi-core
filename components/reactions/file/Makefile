build:
	cargo build -p drasi-reaction-file

test:
	cargo test -p drasi-reaction-file

integration-test:
	cargo test -p drasi-reaction-file --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-file --all-targets --all-features -- -D warnings
