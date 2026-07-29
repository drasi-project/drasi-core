.PHONY: build test integration-test lint fmt

build:
	cargo build -p drasi-reaction-eventgrid

test:
	cargo test -p drasi-reaction-eventgrid --lib

integration-test:
	cargo test -p drasi-reaction-eventgrid --test integration -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-eventgrid --all-targets -- -D warnings

fmt:
	cargo fmt -p drasi-reaction-eventgrid
