# Quine CLI için çok aşamalı (multi-stage) Rust derlemesi.
# Not: Sandbox çalıştırmaları için imaj içinde `rustc` bulunmalıdır
# (LocalProcessSandbox üretilen kodu rustc ile derler). Bu yüzden
# son aşamada rustc/cargo içeren `rust` tabanı kullanılır.

# --- Aşama 1: derleme ---
FROM rust:1-slim AS builder
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY benchmarks ./benchmarks
COPY docs ./docs

RUN cargo build --release --bin quine

# --- Aşama 2: çalışma zamanı ---
FROM rust:1-slim AS runtime
WORKDIR /app

# Sandbox derlemeleri için rustc + cargo zaten mevcut.
COPY --from=builder /src/target/release/quine /usr/local/bin/quine

RUN mkdir -p /app/data
ENV OLLAMA_HOST=http://ollama:11434 \
    QUINE_MODEL=qwen2.5-coder:1.5b \
    QUINE_SANDBOX=local \
    RUST_LOG=info

ENTRYPOINT ["quine"]
CMD ["test-llm"]
