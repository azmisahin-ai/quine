#!/usr/bin/env bash
# Quine geliştirme ortamını tek komutla kurar.
# Kullanım: ./scripts/setup-dev.sh
set -u

echo "=== Quine geliştirme ortamı kurulumu ==="

# 1) Rust toolchain
if ! command -v cargo >/dev/null 2>&1; then
  echo "→ Rust toolchain bulunamadı, rustup kuruluyor…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
else
  echo "✓ cargo mevcut: $(cargo --version)"
fi

# 2) fmt + clippy bileşenleri
rustup component add rustfmt clippy >/dev/null 2>&1 || true

# 3) Ollama (Docker varsa)
if command -v docker >/dev/null 2>&1; then
  echo "→ Ollama konteyneri başlatılıyor…"
  docker compose up -d ollama
  echo "→ Model indiriliyor (qwen2.5-coder:1.5b)…"
  docker compose exec ollama ollama pull "${QUINE_MODEL:-qwen2.5-coder:1.5b}" || true
else
  echo "⚠ Docker bulunamadı. Ollama'yı elle kurun: https://ollama.com/download"
fi

# 4) Derleme + test
echo "→ Workspace derleniyor…"
cargo build --workspace

echo "→ Birim testleri…"
cargo test --workspace

echo ""
echo "✅ Kurulum tamam. Sonraki adım:"
echo "   cargo run --bin quine -- init"
echo "   cargo run --bin quine -- test-llm"
echo "   cargo run --bin quine -- run-once --problem fib-001"
