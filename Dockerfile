# syntax=docker/dockerfile:1

FROM docker.io/library/rust:1-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      clang libclang-dev pkg-config \
      libacl1-dev libcrypt-dev libssl-dev libsystemd-dev libzstd-dev uuid-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && install -Dm755 target/release/siphon /out/siphon

FROM docker.io/library/debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates libacl1 libcrypt1 libssl3t64 libsystemd0 libuuid1 libzstd1 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/siphon /usr/local/bin/siphon
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/siphon"]
