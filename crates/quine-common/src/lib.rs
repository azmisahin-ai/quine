//! # quine-common
//!
//! Quine framework'ünün **paylaşılan veri yapıları**. Bu crate hiçbir iç
//! crate'e bağımlı değildir (bkz. docs/API_CONTRACTS.md).
//!
//! Temel tipler: [`Agent`], [`Problem`], [`EvaluationResult`], [`MutationStrategy`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Bir benchmark problemini çözmeye çalışan ajanın kaydı.
///
/// Faz 2'de `system_prompt` öz-değişiklik döngüsü tarafından güncellenir;
/// Faz 3'te `generation` ve `fitness_score` popülasyon yönetimi için kullanılır.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    /// Evrensel ajan kimliği.
    pub id: Uuid,
    /// Ajanın görünen adı (örn. "fibonacci-solver-v3").
    pub name: String,
    /// LLM'e gönderilen sistem prompt'u — ajanın "genomu".
    pub system_prompt: String,
    /// Ajanın türediği jenerasyon numarası (kök ajan = 0).
    pub generation: u32,
    /// Son değerlendirme turundan gelen fitness skoru (0.0..=100.0).
    pub fitness_score: f64,
    /// Bu ajanın atası (yeni doğan ajanlar için `None`).
    pub parent_id: Option<Uuid>,
    /// Kaydın oluşturulma zamanı.
    pub created_at: DateTime<Utc>,
}

impl Agent {
    /// Varsayılan bir başlangıç prompt'u ile yeni (jenerasyon 0) bir ajan yaratır.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            system_prompt: default_system_prompt().to_string(),
            generation: 0,
            fitness_score: 0.0,
            parent_id: None,
            created_at: Utc::now(),
        }
    }

    /// Ajanın prompt'unu değiştirerek yeni bir mutant (çocuk) ajan üretir.
    pub fn mutate_prompt(&self, new_prompt: String) -> Agent {
        Agent {
            id: Uuid::new_v4(),
            name: format!("{}-g{}", self.name, self.generation + 1),
            system_prompt: new_prompt,
            generation: self.generation + 1,
            fitness_score: 0.0,
            parent_id: Some(self.id),
            created_at: Utc::now(),
        }
    }

    /// Keşif notunu **değiştirerek** prompt'u mutasyona uğratır.
    ///
    /// [`Agent::mutate_prompt`]'ten farkı: önceki jenerasyonlardan kalan
    /// `[keşif notu gN]` satırlarını temizler. Aksi halde ardışık
    /// jenerasyonlarda notlar üst üste birikerek prompt'u şişirir.
    pub fn mutate_prompt_with_hint(&self, hint: &str) -> Agent {
        let base = self
            .system_prompt
            .lines()
            .filter(|line| !line.trim_start().starts_with("[keşif notu g"))
            .collect::<Vec<_>>()
            .join("\n");
        self.mutate_prompt(format!("{}\n{hint}", base.trim_end()))
    }
}

/// Ajanın varsayılan sistem prompt'u (Turkish, code-focused).
pub fn default_system_prompt() -> &'static str {
    "Sen deneyimli bir Rust geliştiricisisin. Sana verilen problemi çözmen \
     istenecek. Yalnızca istenen fonksiyonun gövdesini içeren, derlenebilir \
     ve test edilebilir saf Rust kodu üret. Kod dışında açıklama yapma."
}

/// Tek bir girdi/beklenen çıktı çifti. Değerler JSON olarak tutulur ki
/// sandbox içindeki test koşucusu ile kolayca karşılaştırılabilsinler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestCase {
    pub input: serde_json::Value,
    pub expected: serde_json::Value,
}

/// Benchmark'taki tek bir problem tanımı.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    /// Problems benzersiz kısa kimliği (örn. "fib-001").
    pub id: String,
    /// İnsan okunur problem adı.
    pub title: String,
    /// LLM'e gösterilecek problem açıklaması.
    pub description: String,
    /// LLM'in doldurması gereken fonksiyon imzası (stub).
    pub function_signature: String,
    /// Doğrulama testleri.
    pub test_cases: Vec<TestCase>,
}

impl Problem {
    /// LLM'e gönderilecek tam kullanıcı prompt'unu üretir.
    pub fn to_llm_prompt(&self) -> String {
        format!(
            "# Problem: {}\n\n{}\n\nAşağıdaki fonksiyonu tamamla:\n\n```rust\n{}\n```",
            self.title, self.description, self.function_signature
        )
    }
}

/// Bir değerlendirme turusunun sonucu (Faz 1 kabul kriteri çıktısı budur).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Değerlendirilen problem.
    pub problem_id: String,
    /// Değerlendirmeyi yapan ajan.
    pub agent_id: Uuid,
    /// Tüm testler geçti mi?
    pub success: bool,
    /// 0.0 ..= 100.0 arası puan (geçen test oranı ağırlıklı).
    pub score: f64,
    /// Sandbox'tan yakalanan stdout.
    pub stdout: String,
    /// Sandbox'tan yakalanan stderr (derleme hataları dahil).
    pub stderr: String,
    /// Çalışma süresi (ms).
    pub duration_ms: u64,
    /// Kaç test geçti / toplam test.
    pub tests_passed: usize,
    pub tests_total: usize,
    /// Sonuç üretim zamanı.
    pub evaluated_at: DateTime<Utc>,
}

impl EvaluationResult {
    /// Faz 1 kabul kriterindeki ideal sonuç: `success: true, score: 100.0`.
    pub fn perfect(problem_id: &str, agent_id: Uuid, tests: usize) -> Self {
        Self {
            problem_id: problem_id.to_string(),
            agent_id,
            success: true,
            score: 100.0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
            tests_passed: tests,
            tests_total: tests,
            evaluated_at: Utc::now(),
        }
    }
}

/// Faz 2/4: Ajanın kendisini iyileştirirken izleyebileceği mutasyon stratejileri.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationStrategy {
    /// Başarısızlıkları analiz edip sistem prompt'unu yeniden yaz.
    PromptRefinement,
    /// Ajanın araç setine yeni bir araç ekle (dosya tabanlı, Faz 2+).
    ToolAddition,
    /// Gerçek kaynak kodunu değiştir (Guardian onayından geçer, Faz 4).
    CodeMutation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_new_has_defaults() {
        let a = Agent::new("test-agent");
        assert_eq!(a.generation, 0);
        assert!(!a.system_prompt.is_empty());
        assert!(a.parent_id.is_none());
    }

    #[test]
    fn mutate_prompt_increments_generation_and_links_parent() {
        let parent = Agent::new("parent");
        let child = parent.mutate_prompt("yeni prompt".into());
        assert_eq!(child.generation, parent.generation + 1);
        assert_eq!(child.parent_id, Some(parent.id));
        assert_ne!(child.id, parent.id);
        assert_eq!(child.system_prompt, "yeni prompt");
    }

    #[test]
    fn mutate_prompt_with_hint_replaces_previous_hints() {
        let root = Agent::new("root");
        let g1 = root.mutate_prompt_with_hint("[keşif notu g1] HINT_A");
        let g2 = g1.mutate_prompt_with_hint("[keşif notu g2] HINT_B");
        let g3 = g2.mutate_prompt_with_hint("[keşif notu g3] HINT_C");

        let hint_count = g3
            .system_prompt
            .lines()
            .filter(|l| l.starts_with("[keşif notu g"))
            .count();
        assert_eq!(hint_count, 1, "notlar birikmemeli: {}", g3.system_prompt);
        assert!(g3.system_prompt.contains("HINT_C"));
        assert!(!g3.system_prompt.contains("HINT_A"));
        assert!(!g3.system_prompt.contains("HINT_B"));
        assert_eq!(g3.generation, 3);
    }

    #[test]
    fn agent_roundtrips_through_serde_json() {
        let a = Agent::new("serde-agent");
        let json = serde_json::to_string(&a).expect("serialize");
        let b: Agent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(a, b);
    }

    #[test]
    fn evaluation_result_perfect_is_100() {
        let r = EvaluationResult::perfect("fib-001", Uuid::new_v4(), 3);
        assert!(r.success);
        assert_eq!(r.score, 100.0);
        assert_eq!(r.tests_passed, r.tests_total);
    }

    #[test]
    fn problem_prompt_contains_signature() {
        let p = Problem {
            id: "x-1".into(),
            title: "Test".into(),
            description: "Bir şey yap".into(),
            function_signature: "fn foo() -> u32 { todo!() }".into(),
            test_cases: vec![TestCase {
                input: serde_json::json!(null),
                expected: serde_json::json!(42),
            }],
        };
        let prompt = p.to_llm_prompt();
        assert!(prompt.contains("fn foo()"));
        assert!(prompt.contains("Test"));
    }

    #[test]
    fn mutation_strategy_serializes_snake_case() {
        let s = serde_json::to_string(&MutationStrategy::PromptRefinement).unwrap();
        assert_eq!(s, "\"prompt_refinement\"");
    }
}
