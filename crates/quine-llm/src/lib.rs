//! # quine-llm
//!
//! LLM backend soyutlaması. Çekirdek Prensip #2 gereği **yerel** servislerle
//! (Ollama / llama.cpp / vLLM) konuşur; bulut API'leri bilinçli olarak yoktur.
//!
//! * [`LlmBackend`] — tüm backend'lerin uyacağı trait.
//! * [`OllamaBackend`] — varsayılan implementasyon (`/api/generate`).
//! * [`EchoBackend`] — ağa çıkmeden tam döngüyü test etmek için sahte backend.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// LLM'e gönderilen istek.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Model adı (örn. `qwen2.5-coder:7b`).
    pub model: String,
    /// Sistem prompt'u (ajanın genomu).
    pub system: String,
    /// Kullanıcı mesajı (problem + talimatlar).
    pub prompt: String,
    /// Sampling sıcaklığı.
    pub temperature: f32,
    /// Yanıt için maksimum token.
    pub max_tokens: u32,
}

impl LlmRequest {
    pub fn new(
        model: impl Into<String>,
        system: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            prompt: prompt.into(),
            temperature: 0.2,
            max_tokens: 1024,
        }
    }
}

/// LLM'den dönen yanıt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Ham metin.
    pub content: String,
    /// Yanıtı üreten model.
    pub model: String,
    /// Sunucu tarafında ölçülen süre (ns), varsa.
    pub duration_ns: Option<u64>,
}

impl LlmResponse {
    /// Markdown kod bloklarını soyup saf kod parçalarını birleştirir.
    /// LLM çıktısından derlenecek kaynak kodu çıkarmak için kullanılır (Faz 1).
    pub fn extract_code(&self) -> String {
        let mut out = String::new();
        let mut in_fence = false;
        for line in self.content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                out.push_str(line);
                out.push('\n');
            }
        }
        if out.trim().is_empty() {
            // Hiç fence yoksa ham içeriği döndür.
            self.content.clone()
        } else {
            out
        }
    }
}

/// Tüm LLM backend'leri bu sözleşmeye uyar (docs/API_CONTRACTS.md).
///
/// Trait **dyn-uyumlu** tutulur (Prensip #5: backend seçimi çalışma zamanında):
/// async metotlar `Box::pin` ile kutulanmış future döner; böylece CLI ve
/// evrim motoru `Arc<dyn LlmBackend>` üzerinden paralel değerlendirme yapabilir.
#[allow(async_fn_in_trait)] // kasıtlı: dyn uyumluluğu için boxed future imzaları
pub trait LlmBackend: Send + Sync {
    /// Backend'in kısa adı ("ollama", "echo", ...).
    fn name(&self) -> &str;

    /// Bir üretim isteğini tamamlar.
    fn generate<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<LlmResponse>> + Send + 'a>>;

    /// Backend'e erişilebilir mi? (Faz 0 kabul kriteri bunu kullanır.)
    fn health_check(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;
}

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

/// Varsayılan yerel Ollama sunucusu adresi.
pub const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";
/// Varsayılan kod modeli.
pub const DEFAULT_MODEL: &str = "qwen2.5-coder:7b";

/// Ollama REST API'sine (`/api/generate`) konuşan backend.
#[derive(Debug, Clone)]
pub struct OllamaBackend {
    host: String,
    model: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct OllamaGenerateBody<'a> {
    model: &'a str,
    system: &'a str,
    prompt: &'a str,
    stream: bool,
    options: OllamaOptions<'a>,
}

#[derive(Serialize)]
struct OllamaOptions<'a> {
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a [String]>,
}

#[derive(Deserialize)]
struct OllamaGenerateResp {
    response: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    total_duration: Option<u64>,
}

#[derive(Deserialize)]
struct OllamaTags {
    #[serde(default)]
    models: Vec<OllamaTagModel>,
}

#[derive(Deserialize)]
struct OllamaTagModel {
    name: String,
}

impl OllamaBackend {
    /// Ortam değişkenlerinden (`OLLAMA_HOST`, `QUINE_MODEL`) backend kurar.
    pub fn from_env() -> Self {
        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| DEFAULT_OLLAMA_HOST.to_string());
        let model = std::env::var("QUINE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Self::new(host, model)
    }

    pub fn new(host: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client");
        Self {
            host: host.into(),
            model: model.into(),
            client,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// `/api/tags` üzerinden istenen modelin kurulu olup olmadığını kontrol eder.
    async fn ensure_model_installed(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.host);
        let tags: OllamaTags = self
            .client
            .get(&url)
            .send()
            .await
            .context("Ollama /api/tags isteği başarısız")?
            .error_for_status()?
            .json()
            .await
            .context("Ollama /api/tags yanıtı çözümlenemedi")?;

        if tags.models.iter().any(|m| m.name == self.model) {
            Ok(())
        } else {
            let available: Vec<&str> = tags.models.iter().map(|m| m.name.as_str()).collect();
            Err(anyhow!(
                "Model '{}' Ollama'da kurulu değil. Kurulu modeller: {:?}",
                self.model,
                available
            ))
        }
    }
}

impl LlmBackend for OllamaBackend {
    fn name(&self) -> &str {
        "ollama"
    }

    fn generate<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<LlmResponse>> + Send + 'a>> {
        Box::pin(self.generate_impl(request))
    }

    fn health_check(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(self.health_check_impl())
    }
}

impl OllamaBackend {
    async fn generate_impl(&self, request: &LlmRequest) -> Result<LlmResponse> {
        self.ensure_model_installed().await?;

        let body = OllamaGenerateBody {
            model: &request.model,
            system: &request.system,
            prompt: &request.prompt,
            stream: false,
            options: OllamaOptions {
                temperature: request.temperature,
                num_predict: Some(request.max_tokens),
                stop: None,
            },
        };

        let url = format!("{}/api/generate", self.host);
        let resp: OllamaGenerateResp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Ollama /api/generate isteği başarısız ({url})"))?
            .error_for_status()?
            .json()
            .await
            .context("Ollama /api/generate yanıtı JSON olarak çözümlenemedi")?;

        Ok(LlmResponse {
            content: resp.response,
            model: resp.model.unwrap_or_else(|| request.model.clone()),
            duration_ns: resp.total_duration,
        })
    }

    async fn health_check_impl(&self) -> Result<()> {
        // /api/tags hem servisin ayakta olduğunu hem de model listesini doğrular.
        self.ensure_model_installed().await
    }
}

// ---------------------------------------------------------------------------
// Echo (deterministic, offline test backend)
// ---------------------------------------------------------------------------

/// İsteği yanıtlayan sabit metinli, ağa çıkmayan sahte backend.
/// Entegrasyon testlerinde tam döngüyü (LLM → kod → sandbox) Ollama olmadan
/// çalıştırmak için kullanılır.
#[derive(Debug, Clone)]
pub struct EchoBackend {
    canned_response: String,
}

impl EchoBackend {
    pub fn new(canned_response: impl Into<String>) -> Self {
        Self {
            canned_response: canned_response.into(),
        }
    }

    /// Fibonacci çözümünü dönen hazır test backend'i.
    pub fn fibonacci_solver() -> Self {
        Self::new(
            "```rust\npub fn fibonacci(n: u32) -> u64 {\n    let (mut a, mut b) = (0u64, 1u64);\n    for _ in 0..n {\n        let t = a + b;\n        a = b;\n        b = t;\n    }\n    a\n}\n```",
        )
    }
}

impl LlmBackend for EchoBackend {
    fn name(&self) -> &str {
        "echo"
    }

    fn generate<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<LlmResponse>> + Send + 'a>> {
        Box::pin(async move {
            Ok(LlmResponse {
                content: self.canned_response.clone(),
                model: format!("echo:{}", request.model),
                duration_ns: Some(0),
            })
        })
    }

    fn health_check(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_code_strips_markdown_fences() {
        let r = LlmResponse {
            content: "İşte çözüm:\n```rust\nfn main() {}\n```\nBitti.".into(),
            model: "m".into(),
            duration_ns: None,
        };
        let code = r.extract_code();
        assert_eq!(code.trim(), "fn main() {}");
    }

    #[test]
    fn extract_code_falls_back_to_raw_content() {
        let r = LlmResponse {
            content: "fn x() {}".into(),
            model: "m".into(),
            duration_ns: None,
        };
        assert_eq!(r.extract_code(), "fn x() {}");
    }

    #[tokio::test]
    async fn echo_backend_answers_and_is_healthy() {
        let b = EchoBackend::fibonacci_solver();
        assert_eq!(b.name(), "echo");
        b.health_check().await.expect("echo always healthy");
        let req = LlmRequest::new("fake", "sys", "prompt");
        let resp = b.generate(&req).await.expect("generate");
        assert!(resp.content.contains("fibonacci"));
    }

    #[tokio::test]
    async fn ollama_health_check_fails_fast_without_server() {
        // Bu test internete/Ollama'ya ihtiyaç duymaz: bağlantı reddedilmeli.
        let b = OllamaBackend::new("http://127.0.0.1:1", "nope");
        let err = b.health_check().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Ollama"), "unexpected error: {msg}");
    }
}
