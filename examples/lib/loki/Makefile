compose-up:
	docker compose up -d

compose-down:
	docker compose down

run:
	LOKI_ENDPOINT=http://localhost:3100 cargo run

build:
	cargo build

test:
	cargo test

integration-test:
	@echo "No automated integration test for example crate"

lint:
	cargo clippy --all-targets -- -D warnings
