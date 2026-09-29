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
            temperature: temperature_from_env(),
            max_tokens: 1024,
        }
    }
}

/// Sampling sıcaklığını `QUINE_TEMPERATURE` ortam değişkeninden okur.
///
/// Varsayılan `0.2`. `0.0` verildiğinde model deterministik/tekrarlanabilir
/// çıktı üretir — evrim ölçümlerinde gürültüyü elemek için kullanışlıdır.
/// Geçersiz veya `[0.0, 2.0]` dışı değerler varsayılana düşer.
pub fn temperature_from_env() -> f32 {
    parse_temperature(std::env::var("QUINE_TEMPERATURE").ok().as_deref())
}

fn parse_temperature(raw: Option<&str>) -> f32 {
    raw.and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|t| (0.0..=2.0).contains(t))
        .unwrap_or(0.2)
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
    /// LLM yanıtından derlenecek saf Rust kodunu çıkarır (Faz 1).
    ///
    /// Sıralı strateji:
    /// 1. ` thinking` blokları temizlenir (bazı modeller muhakemesini basar).
    /// 2. Varsa ```` ```rust ```` blokları birleştirilir.
    /// 3. Yoksa herhangi bir ```` ``` ```` bloğu alınır.
    /// 4. Hiç fence yoksa ham metinden ilk fonksiyon tanımı ayıklanır.
    /// 5. Hiçbiri olmazsa ham içerik döndürülür (son çare).
    ///
    /// Dördüncü adım, fence kullanmayan yanıtlarda açıklama metninin
    /// koda karışıp derlemeyi bozmasını engeller.
    pub fn extract_code(&self) -> String {
        let cleaned = strip_think_blocks(&self.content);

        let rust_fenced = collect_fences(&cleaned, Some("rust"));
        if !rust_fenced.trim().is_empty() {
            return rust_fenced;
        }

        let any_fenced = collect_fences(&cleaned, None);
        if !any_fenced.trim().is_empty() {
            return any_fenced;
        }

        if let Some(func) = extract_first_function(&cleaned) {
            return func;
        }

        cleaned
    }
}

/// ` thinking...</think>` / `<thinking>...</thinking>` bloklarını siler.
/// ` thinking...</think>` ve `<thinking>...</thinking>` bloklarini siler.
/// ` thinking...</think>` ve `<thinking>...</thinking>` bloklarini siler.
/// Kapanmamis blok varsa kalan tum icerik atilir.
fn strip_think_blocks(content: &str) -> String {
    // Tag'ler `concat!` ile kurulur; kaynakta ham angle-bracket tag bulunmaz.
    let pairs: [(&str, &str); 2] = [
        (" thinking", " response"),
        (
            concat!("<", "thinking", ">"),
            concat!("</", "thinking", ">"),
        ),
    ];

    let mut out = String::new();
    let mut rest = content;

    'outer: loop {
        let mut earliest: Option<(usize, &str, &str)> = None;
        for (open, close) in pairs.iter() {
            if let Some(pos) = rest.find(open) {
                if earliest.is_none_or(|(best, _, _)| pos < best) {
                    earliest = Some((pos, open, close));
                }
            }
        }

        let Some((start, _open, close)) = earliest else {
            break;
        };

        out.push_str(&rest[..start]);
        match rest[start..].find(close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => {
                rest = "";
                break 'outer;
            }
        }
    }

    out.push_str(rest);
    out
}

/// ``` fence'leri içindeki kodu toplar. `lang` verilirse yalnızca o dil
/// etiketli bloklar alınır (örn. `rust`); `None` ise tüm bloklar.
fn collect_fences(content: &str, lang: Option<&str>) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut is_target = false;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if !in_fence {
                let info = trimmed.trim_start_matches('`').trim().to_ascii_lowercase();
                is_target = match lang {
                    None => true,
                    Some(l) => info.split_whitespace().next() == Some(l),
                };
                in_fence = true;
            } else {
                in_fence = false;
                is_target = false;
            }
            continue;
        }
        if in_fence && is_target {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Ham metinden ilk `fn` tanımını ve gövdesini (süslü parantez dengesiyle) ayıklar.
/// Süslü parantezler string/char literalleri içinde sayılmaz; benchmark
/// problemleri için bu basitleştirme yeterlidir.
fn extract_first_function(content: &str) -> Option<String> {
    const PREFIXES: [&str; 7] = [
        "pub fn ",
        "pub(crate) fn ",
        "pub(super) fn ",
        "pub async fn ",
        "async fn ",
        "fn ",
        "unsafe fn ",
    ];

    let lines: Vec<&str> = content.lines().collect();
    let start = lines
        .iter()
        .position(|l| PREFIXES.iter().any(|p| l.trim_start().starts_with(p)))?;

    let mut depth: i32 = 0;
    let mut started = false;
    let mut out = String::new();

    for line in &lines[start..] {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    started = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        out.push_str(line);
        out.push('\n');
        if started && depth <= 0 {
            return Some(out);
        }
    }

    // Gövdesiz imza (trait metodu gibi) veya kapanmamış blok: güvenli fallback.
    if started {
        Some(out)
    } else {
        None
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
        assert_eq!(r.extract_code().trim(), "fn x() {}");
    }

    #[test]
    fn extract_code_prefers_rust_fence_over_other_languages() {
        let r = LlmResponse {
            content: "Örnek:\n```python\nprint('x')\n```\n```rust\npub fn a() -> u32 { 1 }\n```"
                .into(),
            model: "m".into(),
            duration_ns: None,
        };
        let code = r.extract_code();
        assert!(code.contains("pub fn a"), "got: {code:?}");
        assert!(!code.contains("print"), "python bloğu alınmamalı: {code:?}");
    }

    #[test]
    fn parse_temperature_defaults_and_validates() {
        assert_eq!(parse_temperature(None), 0.2);
        assert_eq!(parse_temperature(Some("0.0")), 0.0);
        assert_eq!(parse_temperature(Some("0.7")), 0.7);
        assert_eq!(parse_temperature(Some(" 1.5 ")), 1.5);
        // Geçersiz / aralık dışı → varsayılan.
        assert_eq!(parse_temperature(Some("abc")), 0.2);
        assert_eq!(parse_temperature(Some("-1")), 0.2);
        assert_eq!(parse_temperature(Some("9.9")), 0.2);
    }

    #[test]
    fn extract_code_strips_think_blocks() {
        let open = concat!("<", "thinking", ">");
        let close = concat!("</", "thinking", ">");
        let content = format!(
            "{open}burada uzun bir muhakeme var{close}\n```rust\npub fn a() -> u32 {{ 1 }}\n```"
        );
        let r = LlmResponse {
            content,
            model: "m".into(),
            duration_ns: None,
        };
        let code = r.extract_code();
        assert_eq!(code.trim(), "pub fn a() -> u32 { 1 }");
        assert!(!code.contains("muhakeme"));
    }

    #[test]
    fn extract_code_strips_qwen_style_think_tags() {
        let r = LlmResponse {
            content: "🤔 thinkingburada muhakeme🤔 response\n```rust\npub fn a() -> u32 { 1 }\n```"
                .into(),
            model: "m".into(),
            duration_ns: None,
        };
        let code = r.extract_code();
        assert_eq!(code.trim(), "pub fn a() -> u32 { 1 }");
        assert!(!code.contains("muhakeme"));
    }

    #[test]
    fn extract_code_pulls_function_from_unfenced_prose() {
        // Regresyon: model fence kullanmadan prose içinde kod verirse, yalnızca
        // fonksiyon ayıklanmalı — açıklama satırları derlemeye karışmamalı.
        let content = "İşte çözüm:\n\npub fn reverse_string(s: &str) -> String {\n    s.chars().rev().collect()\n}\n\nBu kod metni ters çevirir.\n";
        let r = LlmResponse {
            content: content.into(),
            model: "m".into(),
            duration_ns: None,
        };
        let code = r.extract_code();
        assert!(code.contains("pub fn reverse_string"));
        assert!(!code.contains("İşte çözüm"), "prose karışmamalı: {code:?}");
        assert!(
            !code.contains("ters çevirir"),
            "sondaki prose karışmamalı: {code:?}"
        );
    }

    #[test]
    fn extract_code_falls_back_to_raw_when_no_function() {
        let r = LlmResponse {
            content: "Üzgünüm, bu isteği yanıtlayamam.".into(),
            model: "m".into(),
            duration_ns: None,
        };
        assert_eq!(r.extract_code().trim(), "Üzgünüm, bu isteği yanıtlayamam.");
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
