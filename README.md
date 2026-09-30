# 🧬 Quine — Kendi Kendini Yazan Ajan Framework'ü

> **%100 Rust** ile yazılmış, **yerel LLM** (Ollama) üzerinde çalışan ve
> ürettiği kodu bir **sandbox** içinde ampirik olarak doğrulayan, kendi
> kendini geliştiren bir yapay zeka ajan çerçevesi.

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

Quine, başarısız olduğu problemleri analiz edip kendi prompt'unu (genomunu)
evrimleştiren ajanlar üzerine kuruludur. Adını, kendine referans veren
programlar kavramından ("quine") alır.

---

## 🧭 Çekirdek Prensipler

1. **%100 Rust** — Python/Bash wrapper yok. Tüm mantık, benchmark ve araçlar Rust.
2. **Yerel ve İzole** — Varsayılan olarak internete kapalı (air-gapped), yerel
   LLM (Ollama / llama.cpp) ile çalışır. Kod çalıştırma sandbox içindedir.
3. **Ampirik Doğrulama** — Bir kod değişikliği, otomatik test geçmeden "geçerli" sayılmaz.
4. **Modülerlik** — Çekirdek, LLM, güvenlik ve CLI kesin çizgilerle ayrılmış crate'lerdir.
5. **Güvenlik Öncelikli** — Ajanın ürettiği kod ana sisteme doğrudan erişemez;
   `quine-guardian` her değişikliği engeller veya onaylar.

---

## 🗂️ Mimari

```text
quine-cli ──► quine-runtime ──► quine-eval ──► quine-bench-simple
   │               │                 │                │
   ├──► quine-llm  ├──► quine-guardian                │
   │               ├──► quine-storage (SQLite)        │
   └──► quine-web  └────────────► quine-common ◄──────┘
```

| Crate | Görev |
|-------|-------|
| `quine-common` | Paylaşılan tipler: `Agent`, `Problem`, `TestCase`, `EvaluationResult`, `MutationStrategy` |
| `quine-llm` | LLM soyutlaması: `LlmBackend` trait, `OllamaBackend`, `EchoBackend`, `ScriptedBackend` |
| `quine-eval` | Benchmark çalıştırıcı + Sandbox (`LocalProcessSandbox`, `DockerSandbox`) |
| `quine-evolution` | Popülasyon, seçilim (rulet/turnuva), mutasyon, arşiv |
| `quine-guardian` | Güvenlik: `DiffAnalyzer` + audit log |
| `quine-runtime` | Çalıştırma motoru: run/aday yaşam döngüsü, olay veri yolu (SSE), iptal |
| `quine-storage` | SQLite kalıcılığı: run, aday, olay, denetim kaydı, ajan genomu, metrikler |
| `quine-web` | Web kontrol düzlemi: REST API + SSE + gömülü canlı panel (axum) |
| `quine-cli` | Kullanıcı arayüzü (clap) |
| `quine-bench-simple` | Benchmark problemleri: Fibonacci, String Reverse, List Sum |

Detaylı sözleşmeler: [`docs/API_CONTRACTS.md`](docs/API_CONTRACTS.md)
Mimari ve fazlar: [`docs/MASTER_PLAN.md`](docs/MASTER_PLAN.md)
Doğrulama / yürütme kaydı: [`docs/EXECUTION_PLAN.md`](docs/EXECUTION_PLAN.md)

---

## 🚀 Hızlı Başlangıç

### 0. Tek komutla kurulum

```bash
./scripts/setup-dev.sh
```

### 1. Elle kurulum

```bash
# Rust toolchain (yoksa)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add rustfmt clippy

# Ollama (Docker ile)
docker compose up -d ollama
docker compose exec ollama ollama pull qwen2.5-coder:1.5b

# Derle
cargo build --workspace
```

### 2. Doğrula ve çalıştır

```bash
cargo run --bin quine -- init                     # data/ + config
cargo run --bin quine -- doctor                   # ortam sağlık kontrolü (Rust/Docker/Ollama)
cargo run --bin quine -- test-llm                 # LLM bağlantısı (Faz 0)
cargo run --bin quine -- run-once --problem fib-001   # tek problem (Faz 1)
cargo run --bin quine -- evolve --iterations 5        # prompt evrimi (Faz 2)
cargo run --bin quine -- population evolve --generations 10   # popülasyon (Faz 3)
cargo run --bin quine -- guard check dosya.rs         # güvenlik taraması (Faz 4)
cargo run --bin quine -- mutate dosya.rs --instruction "..."   # kod mutasyonu (Faz 4)
```

### 3. Canlı panel (web kontrol düzlemi)

En hızlı yol — hiçbir şey kurmadan paneli açıp ajanın nasıl çalıştığını
izleyin:

```bash
cargo run --bin quine -- serve --demo
```

Bu komut tarayıcıda `http://127.0.0.1:8080` adresini açar. Panelde:

* **Çalıştır** düğmesiyle bir ajan başlatın; adımları (LLM isteği → kod →
  guardian → sandbox → test → puan) **canlı** olarak zaman çizelgesinde görün.
* **Duraklat / Devam / İptal** ile çalışmayı yönetin.
* Geçmiş çalıştırmaları, aday kodları ve ölçümleri inceleyin.

`--demo` modu Ollama ve Docker **gerektirmez**; sabit senaryoyla (ilk deneme
hatalı kod, ikinci deneme düzeltilmiş kod) öğrenme döngüsünü gösterir. Gerçek
modellerle aynı paneli kullanmak için:

```bash
cargo run --bin quine -- serve        # Ollama + (önerilen) Docker sandbox
```

Komut satırından kalıcı çalıştırma ve geçmiş:

```bash
cargo run --bin quine -- run --mode evolve --problem fib-001   # SQLite'a yazar, olayları akıtır
cargo run --bin quine -- history                               # geçmiş çalıştırmalar
```

### 4. LLM'siz (simülasyon) mod

Ollama kurmadan tüm döngüyü deterministik backend ile çalıştırabilirsiniz:

```bash
cargo run --bin quine -- --simulate run-once
cargo run --bin quine -- --simulate evolve --iterations 3
```

### Windows notu

Windows'ta da çalışır (`cargo run --bin quine -- serve --demo`). İki nokta:

* **`local` sandbox `rustc` ister.** Üretilen kod `rustc` ile derlenir; Rust
  kuruluysa sorun yok. Gerçek çalıştırmalar için **Docker Desktop + `docker`
  sandbox önerilir** (izole, ağ kapalı).
* **Panel adresi** `http://127.0.0.1:8080` — tarayıcı otomatik açılmazsa bu
  adresi elle açın.

---

## ⚙️ Yapılandırma (Ortam Değişkenleri)

| Değişken | Varsayılan | Açıklama |
|----------|-----------|----------|
| `OLLAMA_HOST` | `http://localhost:11434` | Ollama sunucu adresi |
| `QUINE_MODEL` | `qwen2.5-coder:1.5b` | Kullanılacak model |
| `QUINE_SANDBOX` | `docker` | `docker` \| `local`. Varsayılan `docker`'dır. `docker` seçiliyken Docker erişilemezse çalışma **durur** (sessizce `local`'e düşülmez — fail-closed). `local` yalnızca geliştirme içindir. |
| `QUINE_TEMPERATURE` | `0.2` | Örnekleme sıcaklığı (`0.0` = deterministik) |
| `RUST_LOG` | `info` | Log seviyesi |

---

## 🧪 Test ve Kalite

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Mevcut durum (2026-09-30): workspace **hatasız derlenir**, **117 test geçer**
(gerçek `quine` ikilisini ayağa kaldıran uçtan uca testler dahil), `fmt` ve
`clippy` temiz. Uçtan uca doğrulandı:

* **Gerçek LLM (Ollama) + Docker sandbox:** `quine run-once --problem rev-002`
  gerçek modelle kodu üretti, guardian'dan geçirdi, izole Docker konteynerinde
  derleyip çalıştırdı ve 4/4 testi geçti (skor 100).
* **Panel (tarayıcı):** demo ve gerçek çalışmalar canlı izlendi; skor grafiği,
  aday kodu + diff görüntüleme, kalıcı genom kartı, toplam istatistikler ve
  çalıştırma geçmişi çalışıyor. Her çalışma **DEMO** ya da **GERÇEK** rozetiyle
  etiketlenir.
* **Güvenlik:** guardian'a karşı 15 saldırılık adversarial test paketi
  (host dosya okuma/yazma, env sızıntısı, path traversal, takma adlı modül
  içe aktarma, alt süreç, ağ erişimi) — hepsi engellenir.

Ayrıntı: `docs/EXECUTION_PLAN.md`.

---

## 📄 Lisans

AGPL-3.0-only — bkz. [LICENSE](LICENSE).
