build:
	cargo build -p drasi-source-neo4j

test:
	cargo test -p drasi-source-neo4j

integration-test:
	cargo test -p drasi-source-neo4j -- --ignored --nocapture

lint:
	cargo clippy -p drasi-source-neo4j --all-targets -- -D warnings
