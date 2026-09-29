# 📜 QUINE: MASTER IMPLEMENTATION PLAN & ARCHITECTURE

> **Proje:** Quine (The Self-Writing Agent Framework)  
> **Repo:** `https://github.com/azmisahin-ai/quine`  
> **Lisans:** AGPL-3.0  
> **Birincil Dil:** %100 Rust  
> **Durum:** Ana Plan (v1.0) - Uygulamaya Hazır

---

## 🧭 1. ÇEKİRDEK PRENSİPLER (Değişmez Kurallar)

Bu projede herhangi bir kod yazılırken veya karar verilirken şu 5 kural asla çiğnenmemelidir:
1. **%100 Rust:** Python, Bash veya başka bir dilde wrapper yok. Tüm mantık, benchmark'lar ve araçlar Rust ile yazılır.
2. **Yerel ve İzole:** Varsayılan olarak internete kapalı (air-gapped), yerel LLM (Ollama/llama.cpp) ile çalışır. Tüm kod çalıştırmaları katı sandbox (Docker/gVisor) içinde yapılır.
3. **Ampirik Doğrulama:** Hiçbir kod değişikliği, otomatik bir test (benchmark) çalıştırılıp başarılı olduğu kanıtlanmadan "geçerli" sayılmaz.
4. **Modülerlik:** Çekirdek (core), LLM arayüzü, güvenlik ve CLI birbirinden kesin çizgilerle ayrılmış crate'lerdir.
5. **Güvenlik Önceliklidir:** Ajanın ürettiği kod, ana sisteme asla doğrudan erişemez. "Guardian" modülü her değişikliği engeller veya onaylar.

---

## 🗂️ 2. REPO DİZİN YAPISI (Nihai Hedef)

```text
quine/
├── Cargo.toml                  # Workspace tanımı
├── docker-compose.yml          # Ollama + Quine orchestration
├── Dockerfile                  # Multi-stage Rust build
├── docs/
│   ├── MASTER_PLAN.md          # BU DOSYA
│   └── API_CONTRACTS.md        # Crate'ler arası trait tanımları
├── crates/
│   ├── quine-common/           # Paylaşılan veri yapıları (Agent, Problem, Result)
│   ├── quine-llm/              # LLM Backend soyutlaması (Ollama, vLLM)
│   ├── quine-eval/             # Benchmark çalıştırıcı ve Sandbox yöneticisi
│   ├── quine-evolution/        # Popülasyon, seçilim ve mutasyon motoru
│   ├── quine-guardian/         # Güvenlik, diff analizi ve kaynak kısıtlama
│   └── quine-cli/              # Kullanıcı arayüzü (main.rs, TUI, komutlar)
├── benchmarks/
│   └── quine-bench-simple/     # Faz 1 için 5-10 basit Rust/Python problemi
├── data/                       # .gitignore'da olmalı (Arşiv, loglar, sqlite)
└── scripts/
    └── setup-dev.sh            # Geliştirme ortamını tek komutla kurar
```

---

## 🗺️ 3. ADIM ADIM UYGULAMA YOL HARİTASI (PHASE BY PHASE)

Projeyi devasa bir bütün olarak değil, **her biri bağımsız olarak derlenip test edilebilen** 5 ana fazda inşa edeceğiz.

### 🟢 FAZ 0: Temel İskelet ve LLM Bağlantısı
**Amaç:** Workspace'ı kurmak, temel veri tiplerini tanımlamak ve yerel LLM ile başarılı bir şekilde iletişim kurduğunu kanıtlamak.

* **Adım 0.1:** `Cargo.toml` (workspace) ve tüm crate'lerin (`quine-common`, `quine-llm`, `quine-cli`) temel `Cargo.toml` dosyalarını oluştur.
* **Adım 0.2:** `quine-common/src/lib.rs` içinde `Agent`, `Problem`, `EvaluationResult` struct'larını ve `serde` trait'lerini yaz.
* **Adım 0.3:** `quine-llm/src/lib.rs` içinde `LlmBackend` trait'ini ve `OllamaBackend` implementasyonunu yaz.
* **Adım 0.4:** `quine-cli/src/main.rs` içinde `clap` kullanarak `quine init` ve `quine test-llm` komutlarını yaz.
* **✅ Faz 0 Kabul Kriteri (Test):** `docker compose up -d ollama` sonrası `cargo run --bin quine -- test-llm` komutu çalıştırıldığında, terminalde Ollama'dan gelen geçerli bir yanıt ve "✅ LLM Bağlantısı Başarılı" mesajı görülmeli.

### 🟡 FAZ 1: Minimal Döngü ve Değerlendirme (Evaluation)
**Amaç:** Tek bir ajanın, basit bir problemi çözmeye çalışması, kodun sandbox'ta çalıştırılması ve sonucun puanlanması.

* **Adım 1.1:** `benchmarks/quine-bench-simple` crate'ini oluştur. İçine 3 basit problem (örn: Fibonacci, String Reverse, List Sum) içeren bir `SimpleBenchmark` struct'ı yaz.
* **Adım 1.2:** `quine-eval/src/lib.rs` içinde `Evaluator` yapısını kur. Bu yapı, LLM'den gelen kodu geçici bir dosyaya yazıp, `std::process::Command` ile (veya basit bir Docker konteyneri içinde) çalıştırmalı ve stdout/stderr'i yakalamalıdır.
* **Adım 1.3:** `quine-cli` içine `quine run-once` komutu ekle. Bu komut: Problemi al -> LLM'e gönder -> Kodu al -> Eval'da çalıştır -> Sonucu yazdır.
* **✅ Faz 1 Kabul Kriteri (Test):** `quine run-once` komutu çalıştırıldığında, LLM tarafından üretilen kodun derlenip/çalıştığı ve `EvaluationResult { success: true, score: 100.0 }` çıktısının terminalde görüldüğü doğrulanmalı.

### 🟠 FAZ 2: Öz-Değişiklik Döngüsü (Mutation Loop)
**Amaç:** Ajanın, başarısız olduğu durumları analiz edip kendi "prompt"unu veya "workflow"unu iyileştirmesi.

* **Adım 2.1:** `quine-common` içine `MutationStrategy` enum'u ekle (`PromptRefinement`, `ToolAddition`).
* **Adım 2.2:** `quine-evolution/src/lib.rs` içinde `MutationEngine` yapısını kur. Başarısız `EvaluationResult`'ları alıp, LLM'e "Bu hatayı düzeltmek için prompt'u nasıl güncellemeliyim?" diye soran bir mantık yaz.
* **Adım 2.3:** `quine-cli` içine `quine evolve --iterations 5` komutu ekle. Bir `for` döngüsü ile: Çalıştır -> Başarısızsa Analiz Et -> Prompt'u Güncelle -> Tekrar Çalıştır döngüsünü kur.
* **✅ Faz 2 Kabul Kriteri (Test):** Başlangıçta başarısız olan bir problem için, 3-4 iterasyon sonunda ajanın prompt'unun değiştiği ve testin `success: true` döndürdüğü loglarda görülmeli.

### 🔴 FAZ 3: Popülasyon ve Arşiv (Open-Ended Evolution)
**Amaç:** Tek bir ajan yerine, birden fazla ajanın (popülasyon) aynı anda denenmesi ve en iyilerinin bir arşivde (SQLite/JSON) saklanması.

* **Adım 3.1:** `quine-evolution` içine `PopulationManager` ve `Archive` yapısını ekle. `Archive`, başarılı ajanları `data/archive.json` veya `data/archive.db`'ye kaydetmeli.
* **Adım 3.2:** Seçilim mekanizmasını ekle: `select_parents()` fonksiyonu, fitness skoruna göre ağırlıklı rastgele seçim (roulette wheel) veya turnuva seçilimi yapmalı.
* **Adım 3.3:** `quine-cli` içine `quine population evolve --generations 10` komutunu ekle. `tokio::spawn` kullanarak birden fazla ajanın değerlendirilmesini paralel hale getir.
* **✅ Faz 3 Kabul Kriteri (Test):** 10 jenerasyon sonunda `data/archive.json` dosyasında, farklı `id` ve `generation` numaralarına sahip, artan `fitness_score` değerlerine sahip en az 5 farklı ajan kaydı olmalı.

### 🟣 FAZ 4: Gerçek Kod Mutasyonu ve Sıkı Güvenlik (Guardian)
**Amaç:** Ajanın sadece prompt'u değil, kendi `tools/` klasöründeki gerçek Rust kodunu değiştirebilmesi ve bunun güvenlik duvarından geçmesi.

* **Adım 4.1:** `quine-guardian/src/lib.rs` içinde `DiffAnalyzer` yaz. LLM'in ürettiği kod diff'inde `unsafe`, `std::fs::remove_dir_all`, `std::process::Command` gibi tehlikeli pattern'leri regex ile tarayıp reddeden bir yapı kur.
* **Adım 4.2:** `quine-eval` içindeki çalıştırma ortamını tam bir Docker konteynerine (veya mümkünse gVisor'a) taşı. Ağ erişimini (`network=none`) kapat, CPU/RAM limitlerini (`cgroups`) uygula.
* **Adım 4.3:** Ajanın kendi `crates/quine-cli/src/tools/custom_tool.rs` gibi bir dosyayı oluşturup, `cargo build` ile derleyebilmesini sağlayan `CodeMutation` stratejisini ekle.
* **✅ Faz 4 Kabul Kriteri (Test):** Ajan, kasıtlı olarak `rm -rf /` içeren bir kod üretmeye çalıştığında `quine-guardian` bunu `SecurityViolation` ile engellemeli ve bu eylem `data/audit.log` dosyasına kaydedilmeli.

---

## 🧪 4. TEST STRATEJİSİ (Nasıl Test Edeceğiz?)

Her fazın sonunda aşağıdaki komutlar çalıştırılacak ve **hepsi yeşil (✅) yanıt vermelidir**:

1. **Derleme ve Lint:**
   ```bash
   cargo fmt --check && cargo clippy -- -D warnings
   ```
2. **Birim Testleri:**
   ```bash
   cargo test --workspace
   ```
3. **Entegrasyon Testi (Faz 1+):**
   ```bash
   cargo test --test integration -- --nocapture
   ```
4. **E2E Senaryo Testi (Manuel):**
   ```bash
   # Temiz bir ortamda sıfırdan başlama testi
   rm -rf data/ && docker compose up -d && cargo run --bin quine -- evolve --iterations 3
   ```

---

## 🤖 5. YAPAY ZEKA İLE ÇALIŞMA PROTOKOLÜ (Prompting Guide)

Bu planı uygularken, Qwen Coder'a (veya bana) **tek seferde her şeyi yapmasını istemeyin**. Aşağıdaki şablonu kullanarak faz faz ilerleyin:

> **Örnek Prompt (Faz 0 için):**
> "Quine projesinin MASTER_PLAN.md dosyasındaki **FAZ 0** adımlarını uygula.
> 1. Workspace `Cargo.toml` dosyasını oluştur.
> 2. `quine-common` ve `quine-llm` crate'lerini belirtilen yapıda kodla.
> 3. `quine-cli` içinde `test-llm` komutunu yaz.
> Sadece bu fazın kodlarını ver, derlenebilir olduğundan emin ol. Faz 0 Kabul Kriterini nasıl test edeceğimi de sona ekle."

> **Örnek Prompt (Faz 1 için):**
> "Şimdi **FAZ 1**'e geçiyoruz. `quine-eval` ve `quine-bench-simple` crate'lerini oluştur. LLM'den gelen kodu geçici bir dosyaya yazıp çalıştıran ve `EvaluationResult` döndüren `Evaluator` yapısını kodla. `quine-cli`'a `run-once` komutunu ekle."

---

## 🚀 6. İLK HAREKETE GEÇİRME (Action Plan)

Başlamak için şu 3 adımı atın:

1. Bu `MASTER_PLAN.md` içeriğini kopyalayıp reponuzda `docs/MASTER_PLAN.md` olarak oluşturun ve commit edin.
2. Bana (veya Qwen Coder'a) şu komutu verin:
   *"MASTER_PLAN.md'deki **FAZ 0**'ı başlat. Gerekli tüm `Cargo.toml` ve `src/lib.rs` / `src/main.rs` dosyalarını tam kod olarak ver."*
3. Verilen kodları dosyalara yapıştırın, `cargo build` çalıştırın ve hataları (varsa) düzeltmesini isteyin.
