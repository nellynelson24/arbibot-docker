FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin bot --bin scanner \
    && cp target/release/bot target/release/scanner /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home arbibot
COPY --from=build /usr/local/bin/bot /usr/local/bin/scanner /usr/local/bin/
USER arbibot
WORKDIR /app
