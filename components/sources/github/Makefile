build:
	cargo build -p drasi-source-github

test:
	cargo test -p drasi-source-github

integration-test:
	cargo test -p drasi-source-github --test integration_test -- --nocapture

lint:
	cargo clippy -p drasi-source-github --all-targets -- -D warnings
