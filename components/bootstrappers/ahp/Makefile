build:
	cargo build -p drasi-bootstrap-ahp

test:
	cargo test -p drasi-bootstrap-ahp

integration-test:
	cargo test -p drasi-bootstrap-ahp --test bootstrap_tests -- --nocapture

lint:
	cargo clippy -p drasi-bootstrap-ahp --all-targets -- -D warnings
