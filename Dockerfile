FROM rust:1.88 AS builder
WORKDIR /source

RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

COPY Cargo.lock .
COPY Cargo.toml .
COPY src src
COPY test_data test_data
RUN cargo install --path .

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /usr/local/cargo/bin/ad_proxy .
COPY ./entrypoint.sh .

ENTRYPOINT ["/app/entrypoint.sh"]
