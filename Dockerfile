# syntax=docker/dockerfile:1

FROM rust:slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        protobuf-compiler \
        libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release -p verification-service

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 1000 --user-group --no-create-home primora

COPY --from=builder /app/target/release/primora-verification /usr/local/bin/primora-verification

USER primora
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/primora-verification"]
