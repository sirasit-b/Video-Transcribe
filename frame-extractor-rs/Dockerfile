FROM rust:1.88-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.toml
COPY src src
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/frame-extractor-rs /usr/local/bin/frame-extractor-rs

ENV HOST=0.0.0.0
ENV PORT=8081

CMD ["frame-extractor-rs"]
