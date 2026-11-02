# syntax=docker/dockerfile:1
FROM rust:1.95.0-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev cmake protobuf-compiler libprotobuf-dev libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/target
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/target \
    cargo build --locked --manifest-path examples/lib/temporal-actions/Cargo.toml \
    && cp /target/debug/temporal-actions-example /usr/local/bin/drasi-demo

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home demo
COPY --from=build /usr/local/bin/drasi-demo /usr/local/bin/drasi-demo
USER demo
CMD ["drasi-demo"]
