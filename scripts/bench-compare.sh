#!/usr/bin/env bash
# Model karşılaştırma ölçümü: aynı problemi N kez çalıştırıp başarı oranını raporlar.
#
# Kullanım:
#   scripts/bench-compare.sh "qwen2.5-coder:1.5b qwen2.5-coder:7b" "fib-001 rev-002" 3
#   scripts/bench-compare.sh "qwen2.5-coder:1.5b" "examples/problems/factorial.json" 3
#
# Ortam: QUINE_TEMPERATURE (varsayılan 0.0), QUINE_BIN (varsayılan ./target/debug/quine)
set -u

MODELS=${1:-"qwen2.5-coder:1.5b"}
TARGETS=${2:-"fib-001 rev-002 sum-003"}
RUNS=${3:-3}
BIN=${QUINE_BIN:-./target/debug/quine}
TEMP=${QUINE_TEMPERATURE:-0.0}

if [ ! -x "$BIN" ]; then
  echo "❌ quine binary bulunamadı: $BIN (cargo build --bin quine)" >&2
  exit 1
fi

problem_flag() {
  case "$1" in
    *.json) echo "--problem-file $1" ;;
    *) echo "--problem $1" ;;
  esac
}

printf '%-24s %-28s %s\n' "MODEL" "PROBLEM" "BAŞARI"
printf '%s\n' "------------------------------------------------------------"

for model in $MODELS; do
  for target in $TARGETS; do
    ok=0
    flag=$(problem_flag "$target")
    for _ in $(seq 1 "$RUNS"); do
      out=$(QUINE_MODEL="$model" QUINE_TEMPERATURE="$TEMP" RUST_LOG=error \
        timeout 300 "$BIN" run-once $flag 2>&1)
      if echo "$out" | grep -q "success\": true"; then
        ok=$((ok + 1))
      fi
    done
    printf '%-24s %-28s %d/%d\n' "$model" "$(basename "$target")" "$ok" "$RUNS"
  done
done
