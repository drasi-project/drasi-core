.PHONY: build lint test integration-test

ORACLE_CLIENT_HOME ?= $(HOME)/oracle/instantclient_23_3
export ORACLE_CLIENT_HOME
export DYLD_LIBRARY_PATH := $(ORACLE_CLIENT_HOME)$(if $(DYLD_LIBRARY_PATH),:$(DYLD_LIBRARY_PATH))

build:
	cargo build -p drasi-source-oracle

lint:
	cargo clippy -p drasi-source-oracle --all-targets --all-features

test:
	cargo test -p drasi-source-oracle

integration-test:
	cargo test -p drasi-source-oracle --test integration_test -- --ignored --nocapture
