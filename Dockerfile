FROM rust:1.98.1-slim-trixie AS base

RUN apt-get update \
    && rm -rf /var/lib/apt/lists/*


FROM base AS builder

RUN rustup component add rustfmt clippy
RUN cargo install cargo-sweep

WORKDIR /workspace
