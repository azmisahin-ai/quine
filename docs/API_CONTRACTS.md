# Crate'ler Arası API Sözleşmeleri (API Contracts)

> Bu doküman, Quine workspace'indeki crate'ler arasındaki **trait** ve **veri tipi**
> sözleşmelerini tanımlar. Bir crate'in public API'si değiştiğinde bu dosya da güncellenmelidir.

## Bağımlılık Grafiği

```text
quine-cli ──► quine-evolution ──► quine-eval ──► quine-bench-simple
   │               │                 │                │
   ├──► quine-llm  ├──► quine-guardian                │
   │               │        │                         │
   └───────────────┴────────┴────► quine-common ◄─────┘
```

Prensip: **`quine-common` hiçbir iç crate'e bağımlı değildir.** Tüm paylaşılan tipler oradadır.

---

## quine-common (Paylaşılan Tipler)

| Tip | Açıklama |
|-----|----------|
| `Agent` | Kimlik, sistem prompt'u, jenerasyon, fitness skoru taşıan ajan kaydı. |
| `Problem` | Benchmark problemi: id, açıklama, hedef fonksiyon imzası, test case'leri. |
| `TestCase` | `input` / `expected` çifti (JSON değerleri). |
| `EvaluationResult` | `success`, `score`, `stdout`, `stderr`, `duration_ms`. |
| `MutationStrategy` | `PromptRefinement` \| `ToolAddition` \| `CodeMutation` (Faz 4). |

## quine-llm

```rust
#[async_trait] // not: async fn in traits (RPITIT) kullanıyoruz; obj-safe gerekirse Box::pin
pub trait LlmBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn generate(&self, request: &LlmRequest) -> Result<LlmResponse>;
    async fn health_check(&self) -> Result<()>;
}
```

Implementasyonlar: `OllamaBackend` (varsayılan), `EchoBackend` (test/simülasyon modu).

## quine-eval

```rust
pub struct Evaluator<S: Sandbox> { /* ... */ }

impl Evaluator<DockerSandbox> { /* Faz 4: katı izolasyon */ }
impl Evaluator<LocalProcessSandbox> { /* Faz 1: lokal rustc + timeout */ }
```

* `Evaluator::evaluate(problem: &Problem, code: &str) -> EvaluationResult`
* `Sandbox` trait'i kod yazma → derleme → çalıştırma → stdout/stderr yakalama sorumluluğunu taşır.

## quine-bench-simple

* `SimpleBenchmark::problems() -> Vec<Problem>` — Fibonacci, String Reverse, List Sum.
* Her problem bir `solution_stub` içerir; LLM yalnızca ilgili fonksiyonu doldurur.

## quine-evolution

* `MutationEngine::refine_prompt(agent, results) -> String` (Faz 2)
* `PopulationManager::select_parents()` — rulet / turnuva seçilimi (Faz 3)
* `Archive` — `data/archive.json` üzerine JSON kalıcılığı (Faz 3)

## quine-guardian

```rust
pub struct DiffAnalyzer { /* tehlikeli pattern listesi */ }
impl DiffAnalyzer {
    pub fn analyze(&self, diff: &str) -> Result<(), SecurityViolation>;
}
```

* `SecurityViolation`: reddedilen pattern ve satır bilgisini taşır.
* Her karar `data/audit.log` dosyasına yazılır (Faz 4 kabul kriteri).

## quine-cli

Komut yüzeyi (clap):

| Komut | Faz | Açıklama |
|-------|-----|----------|
| `quine init` | 0 | `data/` dizinlerini ve config'i hazırlar. |
| `quine test-llm` | 0 | LLM backend'ine ping atar, yanıtı yazdırır. |
| `quine run-once [--problem ID]` | 1 | Problemi çözer, sandbox'ta çalıştırır, puanlar. |
| `quine evolve --iterations N` | 2 | Prompt-refinement döngüsü. |
| `quine population evolve --generations N` | 3 | Paralel popülasyon evrimi + arşiv. |
| `quine guard check <FILE>` | 4 | Guardian diff analizi. |

Ortam değişkenleri: `OLLAMA_HOST` (vars. `http://localhost:11434`), `QUINE_MODEL` (vars. `qwen2.5-coder:7b`), `QUINE_SANDBOX` (`local` | `docker`).
