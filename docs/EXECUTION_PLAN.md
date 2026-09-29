# 📋 QUINE: YÜRÜTME PLANI (EXECUTION PLAN)

> **Durum:** V1.0 — Doğrulama ve Sertleştirme turu
> **Başlangıç:** 2026-09-29
> **Kapsam:** Bu dosya, `docs/MASTER_PLAN.md`'deki mimarinin **gerçekten
> çalıştığını kanıtlamak** ve eksik operasyonel dosyaları tamamlamak için
> izlenen adımları kaydeder.
>
> **Yöntem:** Atomik (tek adım / tek görev) ilerleme. Her adım bir kabul
> kriteriyle kapatılır ve aşağıdaki tabloda işaretlenir.

---

## 🎯 Neden Bu Plan?

`MASTER_PLAN.md` mimariyi ve 5 fazı tanımlar ama şu soruyu yanıtlamaz:
**"Kod gerçekten derleniyor ve çalışıyor mu?"** Bu repoda kod yazıldıktan
sonra hiçbir zaman uçtan uca doğrulanmamıştı (ortamda Rust toolchain yoktu,
`target/` boştu). Bu plan iki hedefi vardır:

1. **Doğrulama:** Mevcut ~2.600 satır kodu derle, test et, gerçek LLM ile çalıştır.
2. **Sertleştirme:** `MASTER_PLAN.md`'de hedeflenen ama eksik olan operasyonel
   dosyaları (`docker-compose.yml`, `Dockerfile`, `scripts/setup-dev.sh`) ekle.

---

## 🧭 Çalışma Protokolü (LLM'ler İçin)

Bir LLM'e (veya yeni bir geliştiriciye) iş verirken:

1. **Önce bağlamı yükle:** Bu dosya + `MASTER_PLAN.md` + `API_CONTRACTS.md`.
2. **Tek adım ver:** "Aşağıdaki tek adımı yap, başka dosyaya dokunma."
3. **Kapıyı çalıştır:** Her adımdan sonra ilgili `cargo check`/`cargo test`/komutu çalıştır.
4. **İşaretle:** Adım geçtiyse tabloda ✅ yap, commit et, sonraki adıma geç.

> ❌ **Yapma:** "Tüm projeyi baştan yaz", "Faz 0-4'ü birden yap".
> Bu, bağlam kopukluğuna ve kırık koda yol açar.

---

## ✅ İLERLEME DURUMU

| # | Adım | Görev | Kabul Kriteri | Durum |
|---|------|-------|---------------|:-----:|
| **A0** | Ortam | Rust toolchain kurulumu | `cargo --version` çalışıyor | ✅ |
| **A1** | Envanter | `cargo check --workspace` | Tüm workspace hatasız derlenir | ✅ |
| **A2** | Doğrulama | `quine-common`, `quine-llm` | Hatasız | ✅ |
| **A3** | Doğrulama | `quine-bench-simple`, `quine-guardian` | Hatasız | ✅ |
| **A4** | Doğrulama | `quine-eval`, `quine-evolution` | Hatasız | ✅ |
| **A5** | CLI | `quine-cli` derleme + `--help` | 8 komut listelenir | ✅ |
| **A6** | Kalite | `cargo fmt` + `clippy -D warnings` | Temiz | ✅ |
| **A7** | Test | `cargo test --workspace` | 33 test geçer | ✅ |
| **A8** | E2E (sim) | `--simulate` ile Faz 1-4 | Tüm komutlar çalışır | ✅ |
| **A9** | E2E (LLM) | Ollama `qwen2.5-coder:1.5b` | `test-llm` + `run-once` çalışır | ✅ |
| **A10** | Dosyalar | `docker-compose.yml`, `Dockerfile`, `scripts/setup-dev.sh` | `docker compose config` geçer | ✅ |
| **A11** | Doküman | `README.md`, `EXECUTION_PLAN.md`, `AGENTS.md` | Okuyan çalıştırabilir | ✅ |

### A6 detayı
- `cargo fmt --check` başlangıçta `quine-bench-simple/src/lib.rs` içinde stil
  uyumsuzlukları raporladı → `cargo fmt` ile düzeltildi.
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı.

### A8 detayı
- `init` → `data/` + `data/config.json` oluşur.
- `--simulate run-once` → `EvaluationResult { success: true, score: 100.0 }`.
- `--simulate evolve --iterations 3` → arşivlenir.
- `--simulate population evolve --generations 3 --size 4` → `data/archive.json`.
- `guard check <evil.rs>` → `SecurityViolation`, `data/audit.log`'a `blocked` yazar.

### A9 detayı
- Ollama Docker konteyneri + `qwen2.5-coder:1.5b` modeli ile:
  - `test-llm` → "✅ LLM Bağlantısı Başarılı".
  - `run-once --problem fib-001` → 100.0 (4/4).
  - `run-once --problem sum-003` → 100.0 (4/4).
  - `run-once --problem rev-002` → küçük modelde **non-deterministik** başarısızlık
    (model bazen kod bloğu dışına açıklama/örnek satırı karıştırıyor). `evolve`
    döngüsü aynı problemde 1. iterasyonda 100.0'a ulaşarak telafi etti.

---

## 🔎 Bilinen Riskler / Takip Edilecekler

| Risk | Açıklama | Öneri |
|------|----------|-------|
| Küçük model kod karıştırması | 1.5B model bazen kod bloğu dışına metin koyar | Daha büyük model (`7b`) kullan veya `extract_code`'u sıkılaştır |
| `extract_code` dayanıklılığı | Kod bloğu yoksa ham içeriği döndürür | Gelecekte: blok içi `pub fn` imzasına göre filtrele |
| Docker sandbox | Varsayılan `local`; `docker` modu gerçek CI'da test edilmeli | `QUINE_SANDBOX=docker` ile E2E koş |
| Think-block temizliği | Bazı modeller `thinking` etiketi basar | `extract_code` içinde temizle |

---

## 📌 Sonraki Faz (Öneri)

- [ ] `QUINE_SANDBOX=docker` ile uçtan uca değerlendirme testi.
- [ ] `cargo test --test integration` entegrasyon test dosyası ekle.
- [ ] `extract_code`'u çok-örnekli/muhafazakâr hale getir + regresyon testi.
- [ ] CI (GitHub Actions): `fmt`, `clippy`, `test`, `--simulate` E2E.
