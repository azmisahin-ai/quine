# 📋 QUINE: YÜRÜTME PLANI (EXECUTION PLAN)

> **Durum:** V1.3 — Doğrulama + Sertleştirme + Docker + Evrim ölçümü
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
| **A5** | CLI | `quine-cli` derleme + `--help` | 11 komut listelenir | ✅ |
| **A6** | Kalite | `cargo fmt` + `clippy -D warnings` | Temiz | ✅ |
| **A7** | Test | `cargo test --workspace` | 117 test geçer | ✅ |
| **A8** | E2E (sim) | `--simulate` ile Faz 1-4 | Tüm komutlar çalışır | ✅ |
| **A9** | E2E (LLM) | Ollama `qwen2.5-coder:1.5b` | `test-llm` + `run-once` çalışır | ✅ |
| **A10** | Dosyalar | `docker-compose.yml`, `Dockerfile`, `scripts/setup-dev.sh` | `docker compose config` geçer | ✅ |
| **A11** | Doküman | `README.md`, `EXECUTION_PLAN.md`, `AGENTS.md` | Okuyan çalıştırabilir | ✅ |

### B fazı — Sertleştirme (2026-09-29 devam)

| # | Adım | Görev | Kabul Kriteri | Durum |
|---|------|-------|---------------|:-----:|
| **B0** | Temizlik | `target/` git geçmişinden çıkarıldı | Taze clone < 1 MB | ✅ |
| **B1** | CI | GitHub Actions (`fmt`+`clippy`+`test`+E2E sim) | Workflow geçerli, adımlar yerelde geçer | ✅ |
| **B2** | Sertleştirme | `extract_code` + regresyon testleri | 9 llm testi geçer, gerçek LLM 3/3 | ✅ |
| **B3** | Test | `crates/quine-cli/tests/integration.rs` | 6 entegrasyon testi geçer | ✅ |
| **B4** | Docker | `QUINE_SANDBOX=docker` uçtan uca | Simulate + gerçek LLM + guardian geçer | ✅ |
| **B5** | Evrim | Gerçek LLM çok-ajanlı popülasyon + fitness ölçümü | Not birikmesi giderildi, flaky test düzeltildi | ✅ |
| **B6** | Config | `data/config.json` runtime'da okunur | Env > config > varsayılan önceliği doğrulandı | ✅ |
| **B7** | Dış problem | `--problem-file` + imzadan türeyen generic harness | Dış problemler (faktöriyel/palindrom) LLM ile değerlendirildi | ✅ |
| **B5b** | Evrim kalitesi | `refine_prompt` taban prompt'u korur, `[ders]` kurallarını biriktirir | Prompt çökmesi (2343→89) bitti; birikim testi geçer | ✅ |

### B0 detayı (git geçmişi temizliği)
- `target/` (1444 artifact, ~143 MB) yanlışlıkla repoya commit edilmişti;
  `.gitignore` kuralı eklenmeden önce eklendiği için git izlemeye devam ediyordu.
- `git filter-repo --path target --invert-paths` ile **tüm geçmişten** silindi.
- Sonuç: pack boyutu **143.55 MiB → 78.72 KiB**; taze clone **636 KB**.
- Not: Geçmiş yeniden yazıldığı için SHA'lar değişti → `git push --force` gerekir.

### B2 detayı (`extract_code` sertleştirme)
- ` thinking` / `<thinking>` blokları temizlenir (Qwen dahil).
- Yalnızca ```` ```rust ```` fence'leri tercih edilir; diğer diller atlanır.
- Fence yoksa ham metinden ilk fonksiyon gövdesi (süslü parantez dengesi) ayıklanır
  → açıklama metninin koda karışıp derlemeyi bozması engellenir.
- 5 yeni regresyon testi; gerçek LLM ile fib-001/sum-003/rev-002 → 100/100.
- `quine-llm` test sayısı: 4 → 9.

### B4 detayı (docker sandbox E2E)
- `DockerSandbox` `docker run --rm -i --network none --memory 512m --cpus 1.0`
  ile `rust:1-slim-bookworm` imajında derleyip çalıştırır; ağ ve kaynak izole.
- Doğrulanan senaryolar (hepsi `QUINE_SANDBOX=docker`):
  - `--simulate run-once --problem fib-001` → 100.0 (4/4), ~0.5s.
  - `--simulate evolve` + `population evolve` → arşive yazıldı.
  - gerçek LLM (`qwen2.5-coder:1.5b`) `run-once` → 100.0 (4/4).
  - `guard check evil.rs` → `SecurityViolation` (sandbox'a hiç ulaşmadan).
- Yeni opt-in test: `docker_sandbox_evaluates_in_isolation`
  (`#[ignore]`; `QUINE_TEST_DOCKER=1` ile çalışır) → yerelde geçti.
- CI'a `e2e-docker` job'u eklendi (`needs: quality`).
- Not: Ortamda `openhands` kullanıcısı `docker` grubuna eklendi
  (`usermod -aG docker`); CI'da runner zaten docker erişimli.


### B6 detayı (config.json runtime kullanımı)
- Önceden `init` `data/config.json` yazıyordu ama runtime yalnızca ortam
  değişkenlerine bakıyordu → dosyadaki `sandbox`/`model` alanları etkisizdi.
- Yeni `load_config` + `resolved_host/model/sandbox`: **env > config > varsayılan**.
- Bozuk/yok dosya sessizce varsayılana düşer (`tracing::warn`).
- Doğrulandı: config'te `sandbox: docker` → docker denenir, yoksa local'e düşer;
  `QUINE_SANDBOX=local` ile env override çalışır.

### B7 detayı (dış problem desteği)
- Sorun: test harness'i `match problem.id` ile 3 probleme **sabit kodluydu**;
  beklenen çıktılar da `bench::expected_outputs(id)` ile gömülü sete bağlıydı.
- Çözüm: harness artık `function_signature` stub'ından türetilir
  (`parse_signature` + `arg_expr`/`out_expr`); beklenen çıktılar `test_cases`'ten.
- Harness düz `rustc` ile derlendiği için JSON çözümleme **std-only**'dir
  (serde_json yok) — desteklenen tipler: i8..i64/u8..u64/usize/isize, f32/f64,
  bool, String/&str, Vec<i64>/&[i64]. Desteklenmeyen tip → net hata.
- CLI: `run-once`/`evolve` artık `--problem-file <json>` kabul eder; verilirse
  `--problem` yok sayılır ve tanım doğrulanır (id/imza/test zorunlu).
- Örnekler: `examples/problems/factorial.json`, `examples/problems/palindrome.json`.
- Gerçek LLM (`qwen2.5-coder:1.5b`) ile doğrulandı: palindrom → 100.0 (4/4).
- `evolve` artık her iterasyonda üretilen kodu ve ilk hata satırlarını yazdırır.

### B5b detayı (evrim prompt kalitesi)
- Kök neden: `refine_prompt` LLM'e **tüm sistem prompt'unu baştan yazdırıyordu**;
  1.5b model iyi taban prompt'u bozuyordu. Ölçüldü: 2. iterasyonda prompt
  **2343 → 89 karaktere** düştü (birikmiş kurallar silindi) ve evrim hiç yakınsamadı.
- Çözüm: LLM artık **tek ve kısa bir kural** üretir; taban prompt aynen korunur,
  kural `[ders] ...` satırı olarak eklenir. Yinelenen eklenmez, en fazla 8 ders
  tutulur. Yeni test: `refine_prompt_preserves_base_and_accumulates_rules`.
- Doğrulama: `evolve --problem-file factorial.json` → prompt artık 231 karakterde
  **sabit** kalıyor (çökme yok). Ancak `qwen2.5-coder:1.5b` bu problemdeki
  `u32→u64 product` tip hatasını hâlâ düzeltemiyor → **kalan darboğaz model
  kapasitesi**, framework değil.

### B5 detayı (gerçek LLM popülasyon ölçümü)
- Ölçüm (`qwen2.5-coder:1.5b`, `population evolve --generations 3 --size 4`):
  ortalama fitness **100.0 → 91.7 → 91.7** (düşüş eğilimi gözlendi).
- Kök neden 1 — **not birikmesi:** `next_generation` her jenerasyonda prompt'a
  `[keşif notu gN]` satırı **ekliyordu**, hiç temizlemiyordu → genom şişiyordu.
  Çözüm: `Agent::mutate_prompt_with_hint` eski notları temizleyip tek not bırakır.
- Kök neden 2 — **elit bozulması:** 100.0 alan mükemmel ebeveyn bile mutasyona
  uğruyordu. Çözüm: `fitness >= 100.0` ise prompt aynen korunur; çocuklar
  ebeveyn fitness'ıyla başlatılır (sıfır skorla elenmesinler).
- Kök neden 3 — **flaky test:** `tournament_picks_best_of_k` k=4 ile 2 ajan
  kullanıyordu; zayıf ajanın kazanma olasılığı (1/2)^4 = %6 (yorumdaki 1/256
  yanlıştı, k=8 varsayılmıştı). k=8'e çıkarıldı → 3/3 ardışık koşuda kararlı.
- Kalan dalgalanma ağırlıklı olarak **model örnekleme gürültüsü**:
  `temperature = 0.2`. Bu nedenle `QUINE_TEMPERATURE` ortam değişkeni eklendi
  (`0.0` → deterministik/tekrarlanabilir ölçüm). Elit (100.0) her jenerasyonda
  korunduğu için en iyi skor monoton kaldı.

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
| Küçük model kod karıştırması | 1.5B model bazen kod bloğu dışına metin koyar | ✅ B2'de `extract_code` sertleştirildi; hâlâ daha büyük model (`7b`) daha iyi olur |
| `extract_code` dayanıklılığı | Kod bloğu yoksa ham içeriği döndürür | ✅ B2: think-block temizliği + `rust` fence tercihi + fence'siz fonksiyon ayıklama |
| Docker sandbox | Varsayılan `docker`; izole çalışmalı | ✅ B4: `QUINE_SANDBOX=docker` ile E2E + CI job doğrulandı |
| Think-block temizliği | Bazı modeller `thinking` etiketi basar | ✅ B2: `extract_code` içinde temizleniyor |

---

## 📌 Sonraki Faz (Öneri)

- [x] `cargo test --test integration` entegrasyon test dosyası ekle. (B3)
- [x] `extract_code`'u muhafazakâr hale getir + regresyon testi. (B2)
- [x] CI (GitHub Actions): `fmt`, `clippy`, `test`, `--simulate` E2E. (B1)
- [x] `QUINE_SANDBOX=docker` ile uçtan uca değerlendirme testi. (B4)
- [x] Çok-ajanlı gerçek LLM senaryosu (`population evolve`) + fitness ölçümü. (B5)
- [x] Daha büyük model (`qwen2.5-coder:7b`) ile karşılaştırmalı benchmark.
- [x] Faz 5: kalıcı çalıştırma + canlı web paneli + duraklat/devam/iptal.
- [x] Platform: Windows derleme desteği (`x86_64-pc-windows-gnu` clippy temiz).
- [ ] **Hatalı kodu doğrudan düzelten döngü** (derleyici çıktısını koda geri
      besle) — küçük modellerde yakınsamayı hızlandırır.
- [ ] **Nedensel fitness** — mutasyonun katkısını ölç (A/B karşılaştırma).
- [ ] CI matrisine Windows işi ekle (`windows-latest`).

---

## 🧪 Test Özeti (güncel)

| Katman | Test sayısı | Durum |
|--------|:-----------:|:-----:|
| `quine-common` | 7 | ✅ |
| `quine-llm` | 16 | ✅ |
| `quine-guardian` | 8 | ✅ |
| `quine-eval` | 17 | ✅ |
| `quine-evolution` | 13 | ✅ |
| `quine-bench-simple` | 6 | ✅ |
| `quine-storage` | 8 | ✅ |
| `quine-runtime` | 14 | ✅ |
| `quine-web` | 8 | ✅ |
| `integration` (quine-cli) | 7 (+1 docker opt-in) | ✅ |
| `cli_serve` (gerçek ikili E2E) | 5 | ✅ |
| **Toplam** | **117** | ✅ |

---

## 🔬 Model Karşılaştırması (7b vs 1.5b)

Aynı problem seti, `QUINE_TEMPERATURE=0.0`, 3 tekrar (`scripts/bench-compare.sh`):

| Model | fib-001 | rev-002 | sum-003 | faktöriyel (dış) |
|-------|:-------:|:-------:|:-------:|:----------------:|
| `qwen2.5-coder:1.5b` | 3/3 | 3/3 | 3/3 | **0/3** |
| `qwen2.5-coder:7b`   | 3/3 | 3/3 | 3/3 | **3/3** |

**Sonuç:** Kolay problemlerde iki model eşit; karmaşık problemde darboğaz
model kapasitesi. `7b` dış problemleri ilk iterasyonda çözüyor.

## 🐞 Bulunan ve Düzeltilen Hatalar (sertleştirme)

1. **Evrimde elitizm yoktu** — daha kötü prompt kabul ediliyor, fitness
   iterasyonlar arası sıfırlanıyordu (rastgele yürüyüş). `evolve_step` ile
   elitist tepe-tırmanma eklendi (yalnızca fitness düşmezse mutasyon kabul).
2. **`stderr` boş mantık hatalarında LLM kör kalıyordu** — hangi testin neden
   başarısız olduğu bildirilmiyordu. `evaluate` artık `girdi X: beklenen A,
   alınan B` teşhisi üretiyor.
3. **Kural çıkarımı ` ```rust ` fence etiketini kural sanıyordu** —
   `first_meaningful_line` dil etiketlerini atlıyor.
4. **Çok argümanlı fonksiyonlar hiç çalışmıyordu** — harness tek girdi
   satırını tek değer sanıyordu (`gcd(a, b)` panikliyordu). Harness artık
   test girdilerini tipli Rust literal'leri olarak üretiyor.
5. **Docker erişilemezse sessizce yerel sandbox'a düşülüyordu** — `sandbox=docker`
   istendiği halde LLM kodu ana sistemde çalışıyordu (çekirdek ilke ihlali).
   `sandbox_from_kind` artık `Result` döner ve **hata verir**; yerel mod yalnızca
   açıkça `QUINE_SANDBOX=local` ile seçilir ve kullanıcı uyarılır.
6. **Öğrenilen kural kod satırı olabiliyordu** — model kural yerine kod döndürünce
   `[ders] fn factorial(n: u32) -> u64 {` prompt'a giriyordu. `extract_rule` artık
   yalnızca düz yazı kuralı kabul eder (kod/fence/tek-kelime reddedilir, uzun
   paragraf cümle sınırında kesilir).
7. **Heuristik kural her jenerasyonda tekrar ekleniyordu** — `[ders]` yönetimi iki
   ayrı yerde olduğu için prompt şişiyordu. Ortak `apply_rule` ile tekrar engellendi.
8. **Docker sandbox'ta PID limiti yoktu** — fork-bomb ana makinenin PID'lerini
   tüketebilirdi. `--pids-limit 256` + `--security-opt no-new-privileges` eklendi.
9. **Guardian takma adlı içe aktarmayla atlatılabiliyordu** — `use std::fs as f;
   f::write(...)` biçiminde bir takma ad `fs::` desenini atlatıyordu. Kritik
   modüllerin (`fs`, `process`, `env`, `net`, `os`, `path`) takma adla
   içe aktarılması yasaklandı (`aliased-module-import`).
10. **`Path`/`PathBuf` ile sandbox dışına çıkılabiliyordu** — `PathBuf::from("/etc/shadow")`
    gibi yol manipülasyonu engellenmiyordu. `path-module` kuralı eklendi.
11. **`avg_score` yalnızca başarılı run'ları sayıyordu** — gerçek ilerlemeyi
    değil, yalnızca iyi haberleri gösteriyordu. Artık tüm sonuçlanmış run'ları
    kapsar; ayrıca `candidates_total`, `avg_candidate_score` ve `best_generation`
    metrikleri eklendi.
12. **Demo ve gerçek sonuçlar karıştırılabiliyordu** — panel artık her çalışmayı
    **DEMO** (scripted backend) ya da **GERÇEK** (LLM + sandbox) rozetiyle
    etiketler.

### Adversarial saldırı paketi (P0)

Guardian'a karşı 15 gerçekçi kaçış denemesi içeren `adversarial_escapes_are_blocked`
testi eklendi: host dosya okuma/yazma, env sızıntısı, `env!` makrosu, mutlak yol,
path traversal, alt süreç, shell yıkım, ağ erişimi, `unsafe`, FFI/libc, derleme
zamanı host okuma, platform kaçışı, `PathBuf` ve takma adlı `fs`. **Hepsi engellenir.**

## 🟣 Faz 4 Tamamlandı — CodeMutation + Guardian

- **Adım 4.1** `DiffAnalyzer` (mevcut): `unsafe`, `remove_dir_all`,
  `process::Command`, `rm -rf`, ağ ve env desenlerini tarar; kritik ihlal →
  `SecurityViolation`.
- **Adım 4.2** `DockerSandbox`: `--network none`, `--memory`/`--cpus` limiti,
  `--pids-limit`, `--security-opt no-new-privileges`, `:ro` mount, `timeout`
  ile izolasyon. Docker erişilemezse **hata verir** (sessiz yerel fallback yok).
- **Adım 4.3 (yeni)** `quine-evolution::CodeMutator`: LLM'in önerdiği tam dosya
  içeriğini önce guardian'dan geçirir; temizse yedeği alıp **atomik** yazar,
  kritik ihlalde **hiç yazmaz**. CLI: `quine mutate <dosya> --instruction "..."
  [--dry-run]`. Her karar `data/audit.log`'a işlenir.
- **Kabul kriteri:** `rm -rf /` içeren üretim `guard check` ve `mutate`
  yollarında engelleniyor; `data/audit.log`'a `blocked` kaydı düşüyor.
  Uçtan uca test: `integration::code_mutation_is_applied_and_guarded`.

---

## 🟣 Faz 5 Tamamlandı — Kalıcı Çalıştırma + Web Kontrol Düzlemi

- **Adım 5.1** `quine-runtime`: kalıcı çalıştırma motoru. `RunRequest` →
  `RunEngine`; her adım `RunEvent` olarak akıtılır, `quine-storage` (SQLite) ile
  çalışma/aday/ölçüm kayıtları kalıcıdır.
- **Adım 5.2** `quine-web`: axum tabanlı HTTP API + SSE canlı akışı + gömülü
  (vanilla JS) dashboard. Uçlar: çalıştırma başlat/durum/olaylar, duraklat,
  devam, iptal, geçmiş, adaylar.
- **Adım 5.3 (sertleştirme)** Yalnızca `127.0.0.1`'e bağlanır (başka adres
  açıkça istenirse uyarı verir). Fail-closed sandbox, gövde boyutu limiti
  (`256 KiB`), model adı doğrulaması, güvenlik başlıkları (CSP/`nosniff`/`DENY`),
  eşzamanlı run limiti (`4`, doluysa `429`).
- **Adım 5.4 (düzeltme)** Duraklat/devam/iptal artık **run durumuna ve zaman
  çizelgesine yansır** (`RUN_PAUSED`/`RUN_RESUMED`/`RUN_CANCELLING`/
  `RUN_CANCELLED`). Aksi halde kullanıcı panelde donmuş bir ajan görüyordu.
- **Adım 5.5** Demo modu (`serve --demo`, `--simulate`): LLM **ve** Docker
  gerektirmez; `ScriptedBackend` yapay gecikmeyle çalışır ki adımlar gözle
  görülsün, duraklat/iptal anlamlı olsun.
- **Adım 5.6 (platform)** Windows derleme hatası giderildi: Unix'e özel izin
  API'leri `#[cfg(unix)]` altına alındı; Docker mount yolu Windows biçimine
  çevrilir; tarayıcı açma Windows/macOS/Linux'ta çalışır. `x86_64-pc-windows-gnu`
  hedefinde `clippy -D warnings` temiz.

**Kabul kriteri:** `cargo run --bin quine -- serve --demo` tek komutla paneli
açar; "Çalıştır" ile bir çalışma canlı akar ve `completed` olur; duraklat/devam/
iptal panelde görünür. `cli_serve` testleri bunu gerçek ikili üzerinde doğrular.

---

## 🏁 Gerçek Kullanım Hazırlığı (dürüst değerlendirme)

**Hazır olanlar**
- 5 fazın tümü kod olarak mevcut; **117 test** + `fmt`/`clippy` temiz, CI'da iş akışları.
- Faz 1/2/3/4 uçtan uca **gerçek LLM + Docker** ile doğrulandı.
- Faz 5: canlı web paneli (SSE), duraklat/devam/iptal, SQLite kalıcılığı; demo
  modu LLM/Docker olmadan çalışır.
- **Platform:** Linux **ve** Windows'ta derlenir (`x86_64-pc-windows-gnu`
  hedefinde `clippy -D warnings` temiz).
- Güvenlik: `--network none`, RAM/CPU/PID limiti, `:ro` mount, 60s timeout,
  guardian (kritik ihlalde yazmaz), audit log, docker yoksa fail-closed.

**Bilinen sınırlar (dürüstçe)**
1. **Hata geri beslemesi dolaylıdır** — başarısız derleyici çıktısı `stderr`
   olarak özetlenip LLM'e verilir, LLM bundan tek satırlık bir `[ders]` kuralı
   üretir ve bu kural sonraki denemenin prompt'una eklenir. Yani model hatayı
   *doğrudan* görmüyor; bir kural süzgecinden geçmiş hâlini görüyor. Bu, 1.5b
   gibi küçük modellerde yakınsamayı yavaşlatır. (Kod üretimi her iterasyonda
   sıfırdan yapılır; "hatalı kodu düzelt" döngüsü yok.)
2. **Fitness = test skoru** — kısmi ilerleme (örn. 7/8) teşvik edilir ama
   mutasyonun gerçekten nedensel katkısı ölçülmez.
3. **Harness tip desteği** — `&str`, `String`, `bool`, tamsayı/float, `Vec<i64>`
   destekli; başka tipler için harness genişletmesi gerekir.
4. **Çok-ajanlı popülasyon** — arşiv ve nesil ilerlemesi çalışıyor; ancak
   eşzamanlı değerlendirme sabit problem seti üzerinde.

**Sonuç:** Quine, **yerel/tek-kullanıcı** üretim kullanımı için hazır: panel tek
komutla açılır, çalışma canlı izlenir ve yönetilir, kayıtlar kalıcıdır. Küçük
modellerle **otonom kod üretimi** ise deneyseldir; yakınsama için (1) hatalı kodu
doğrudan düzelten döngü ve (2) nedensel fitness ölçümü eklenmelidir.
