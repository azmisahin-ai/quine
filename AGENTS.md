# AGENTS.md — Quine Repo Rehberi

Bu dosya, bu repoda çalışan ajanlar/geliştiriciler için kalıcı bağlamdır.

## Proje Nedir

Quine: %100 Rust, Cargo workspace tabanlı, yerel LLM (Ollama) ile çalışan ve
üretilen kodu sandbox'ta ampirik olarak doğrulayan kendi kendini geliştiren
ajan framework'ü. Mimari: `docs/MASTER_PLAN.md`, sözleşmeler: `docs/API_CONTRACTS.md`.

## Değişmez Kurallar

1. **Sadece Rust** — başka dilde wrapper/araç ekleme.
2. **Yerel LLM** — bulut API'si ekleme; LLM erişimi `quine-llm` üzerinden.
3. **Ampirik doğrulama** — bir değişiklik test/derleme geçmeden "bitti" sayılmaz.
4. **Modülerlik** — `quine-common` hiçbir iç crate'e bağımlı değildir; bağımlılık
   yönü tek yönlüdür (aşağıdaki grafiğe uy).
5. **Güvenlik** — ajan çıktısı `quine-guardian`'dan geçmeden çalıştırılmaz.

## Bağımlılık Yönü (bozmadan koru)

```text
quine-common            (bağımsız, yalnızca dış crate'ler)
quine-llm        ─┐
quine-guardian   ─┼──► quine-common
quine-bench-simple┘
quine-eval       ──► common, bench-simple, guardian
quine-evolution  ──► common, llm, eval, bench-simple
quine-cli        ──► hepsi
```

## Sık Kullanılan Komutlar

```bash
# Derleme
cargo check --workspace
cargo build --workspace

# Kalite kapısı (PR öncesi yeşil olmalı)
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# LLM'siz uçtan uca (hızlı, deterministik)
cargo run --bin quine -- --simulate run-once
cargo run --bin quine -- --simulate evolve --iterations 3
cargo run --bin quine -- --simulate population evolve --generations 3 --size 4
cargo run --bin quine -- guard check <dosya.rs>

# Ollama ile gerçek LLM
docker compose up -d ollama
docker compose exec ollama ollama pull qwen2.5-coder:1.5b
QUINE_MODEL=qwen2.5-coder:1.5b cargo run --bin quine -- test-llm
QUINE_MODEL=qwen2.5-coder:1.5b cargo run --bin quine -- run-once --problem fib-001
```

## Kod Stili

- `rustfmt` (varsayılan ayarlar) — `cargo fmt` zorunlu.
- `clippy` uyarıları hata sayılır (`-D warnings`).
- Hata yönetimi: `anyhow::Result`, bağlam için `.context(...)`.
- Async: `tokio`; trait'ler dyn-uyumlu tutulur (`Pin<Box<dyn Future>>`).
- Yorumlar yalnızca sezgisel olmayan kararları açıklar; kod tekrarını yapmaz.

## Benchmark Problemleri

| id | Fonksiyon | Tip |
|----|-----------|-----|
| `fib-001` | `pub fn fibonacci(n: u32) -> u64` | u32 → u64 |
| `rev-002` | `pub fn reverse_string(s: &str) -> String` | String |
| `sum-003` | `pub fn list_sum(xs: &[i64]) -> i64` | Vec<i64> |

Yeni problem eklerken `quine-bench-simple` içinde hem çözücü stub'ı hem
`harness_source`/`expected_outputs` girdilerini güncelle ve testini ekle.

## Bilinen Tuzaklar

- Küçük modeller (`1.5b`) bazen kod bloğu dışına açıklama/örnek satırı koyar;
  `quine-llm::LlmResponse::extract_code` ` thinking`/`<thinking>` bloklarını
  temizler, ```` ```rust ```` fence'lerini tercih eder, yoksa ham metinden ilk
  fonksiyon gövdesini (süslü parantez dengesi) ayıklar.
- `data/` dizini `.gitignore`'dadır; içerik üretir, commit etme.
- Docker sandbox varsayılan değildir (`local`); `QUINE_SANDBOX=docker` ile seç.
  Docker grubu yoksa `LocalProcessSandbox`'a düşer (uyarı basar).
- **Evrim mutasyonu:** `next_generation` `[keşif notu gN]` satırlarını
  `mutate_prompt_with_hint` ile temizler — yeni satır eklemek yerine değiştir,
  aksi halde genom jenerasyonlar boyunca şişer. Fitness 100.0 olan ebeveynin
  prompt'u mutasyona uğratılmaz (elit bozulmasını önler).
- **LLM non-determinizmi:** `temperature` varsayılanı `0.2`; ölçüm/benchmark
  tekrarlanabilirliği için `QUINE_TEMPERATURE=0.0` ver.

## Ortam Değişkenleri

| Değişken | Varsayılan | Açıklama |
|----------|-----------|----------|
| `QUINE_MODEL` | `qwen2.5-coder:1.5b` | Ollama modeli |
| `QUINE_SANDBOX` | `local` | `local` veya `docker` |
| `QUINE_TEMPERATURE` | `0.2` | Sampling; `0.0` = deterministik |
| `QUINE_TEST_DOCKER` | (yok) | `1` → docker entegrasyon testi çalışır |
| `OLLAMA_HOST` | `http://localhost:11434` | Ollama adresi |

## Devam Eden / Sonraki İşler

`docs/EXECUTION_PLAN.md` → "Sonraki Faz" bölümüne bak. B0–B5 tamamlandı
(git temizliği, CI, `extract_code`, entegrasyon testleri, docker E2E, evrim
ölçümü). Açık: daha büyük model (`7b`) ile karşılaştırmalı benchmark.
