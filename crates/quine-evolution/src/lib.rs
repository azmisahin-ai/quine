//! # quine-evolution
//!
//! Faz 2: [`MutationEngine`] — başarısız değerlendirmeleri LLM'e analiz ettirip
//! ajanın sistem prompt'unu (genomunu) iyileştirir.
//!
//! Faz 3: [`PopulationManager`] + [`Archive`] — rulet tekerleği seçilimi ve
//! `data/archive.json` üzerinde kalıcı arşiv; değerlendirme `tokio::spawn`
//! ile paraleldir.

use anyhow::{Context, Result};
use quine_common::{Agent, EvaluationResult, Problem};
use quine_llm::{LlmBackend, LlmRequest};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// Başarısızlık analizi yapıp yeni prompt üreten motor (Adım 2.2).
pub struct MutationEngine<'a, B: LlmBackend + ?Sized> {
    llm: &'a B,
}

impl<'a, B: LlmBackend + ?Sized> MutationEngine<'a, B> {
    pub fn new(llm: &'a B) -> Self {
        Self { llm }
    }

    /// "Bu hatayı düzeltmek için hangi kural eklenmeli?" sorgusunu LLM'e sorar
    /// ve ajanın **mevcut prompt'unu koruyarak** yeni kuralı ekler.
    ///
    /// Tüm prompt'u yeniden yazdırmak yerine tek bir kısa kural eklenir; bu,
    /// küçük modellerin iyi taban prompt'u bozmasını (prompt'un çökmesini)
    /// engeller ve öğrenilen derslerin birikmesini sağlar.
    pub async fn refine_prompt(
        &self,
        agent: &Agent,
        model: &str,
        failures: &[EvaluationResult],
    ) -> Result<String> {
        let failure_report = summarize_failures(failures);
        let request = LlmRequest::new(
            model,
            "Sen bir prompt mühendisisin. Bir kodlama ajanının başarısızlığını \
             analiz edip, aynı hatayı bir daha yapmaması için TEK ve KISA bir Rust \
             kuralı yazarsın. Yalnızca kural cümlesini döndür (tek satır, 200 \
             karakterden kısa); açıklama veya markdown işareti kullanma.",
            format!(
                "# Son Değerlendirme Raporu\n{}\n\n\
                 Bu hatayı önleyecek tek, somut ve kısa bir Rust kuralı yaz.",
                failure_report
            ),
        );

        let resp = self
            .llm
            .generate(&request)
            .await
            .context("mutasyon LLM isteği")?;
        let rule = extract_rule(&resp.content)
            .ok_or_else(|| anyhow::anyhow!("LLM geçerli bir kural döndürmedi"))?;

        Ok(apply_rule(agent, &rule))
    }
}

/// Ajanın taban prompt'unu koruyarak bir `[ders]` kuralı ekler.
///
/// Kural yinelenmez ve en fazla 8 kural tutulur (eskiler düşer). Hem LLM'in
/// ürettiği kural hem de heuristik yedek kural buradan geçer; böylece ikinci
/// yol da tekrar biriktirmez.
pub(crate) fn apply_rule(agent: &Agent, rule: &str) -> String {
    let new_rule = format!("[ders] {}", rule.trim());
    let mut rules: Vec<String> = agent
        .system_prompt
        .lines()
        .filter(|l| l.trim_start().starts_with("[ders]"))
        .map(|l| l.trim().to_string())
        .collect();
    if !rules.contains(&new_rule) {
        rules.push(new_rule);
    }
    if rules.len() > 8 {
        rules.drain(0..rules.len() - 8);
    }
    let base: String = agent
        .system_prompt
        .lines()
        .filter(|l| !l.trim_start().starts_with("[ders]"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut updated = base.trim_end().to_string();
    if !rules.is_empty() {
        updated.push('\n');
        updated.push_str(&rules.join("\n"));
    }
    updated
}

fn summarize_failures(results: &[EvaluationResult]) -> String {
    if results.is_empty() {
        return "(başarısız kayıt yok)".to_string();
    }
    results
        .iter()
        .map(|r| {
            format!(
                "- problem {}: {} / {} test geçti, score {:.1}. stderr:\n{}",
                r.problem_id,
                r.tests_passed,
                r.tests_total,
                r.score,
                truncate(&r.stderr, 800)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// LLM'in döndürdüğü metinden tek, kısa, **düz yazı** bir kural çıkarır.
///
/// Kod parçaları (`fn ...`, `{`, `}`, `->`, `;`), markdown fence etiketleri ve
/// boş satırlar elenir. Küçük modeller sık sık kural yerine kod ya da uzun bir
/// paragraf döndürür; bunlar prompt'a `[ders]` olarak eklenirse modeli daha da
/// bozar. Geçerli bir kural bulunamazsa `None` döner (çağıran heuristiğe düşer).
fn extract_rule(raw: &str) -> Option<String> {
    const LANG_TAGS: [&str; 8] = [
        "rust",
        "text",
        "md",
        "markdown",
        "bash",
        "sh",
        "plaintext",
        "json",
    ];
    for line in raw.lines() {
        let l = line
            .trim()
            .trim_start_matches('#')
            .trim()
            .trim_matches('`')
            .trim();
        if l.is_empty() || LANG_TAGS.contains(&l.to_ascii_lowercase().as_str()) {
            continue;
        }
        if looks_like_code(l) {
            continue;
        }
        let rule = first_sentence(l);
        // Gerçek bir kural en az birkaç kelimelik cümledir; `(1..=n).product()`
        // gibi tek "kelimelik" ifadeler kural değildir.
        if rule.chars().count() >= 10 && rule.split_whitespace().count() >= 3 {
            return Some(rule);
        }
    }
    None
}

/// Güçlü kod işaretleri taşıyan satırları ayıklar (prose kural bunları içermez).
fn looks_like_code(l: &str) -> bool {
    const MARKERS: [&str; 8] = ["fn ", "pub ", "impl ", "{", "}", "->", ";", "=>"];
    MARKERS.iter().any(|m| l.contains(m))
}

/// Metnin ilk cümlesini döner; cümle yoksa kelime sınırında 160 karaktere kisar.
/// Uzun paragrafların ortadan kesilip anlamsız kural olmasını engeller.
fn first_sentence(l: &str) -> String {
    // Yalnızca cümle *ortasında* kes ("bir şey. İkinci cümle"); sondaki nokta
    // kuralın parçası olarak kalır.
    let cut = l.find(". ").map(|i| i + 1).unwrap_or(l.len());
    let s = l[..cut].trim();
    if s.chars().count() <= 160 {
        return s.to_string();
    }
    let mut out = String::new();
    for word in s.split_whitespace() {
        if out.chars().count() + word.chars().count() + 1 > 160 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

// ---------------------------------------------------------------------------
// Archive (Adım 3.1)
// ---------------------------------------------------------------------------

/// Arşiv kaydı: başarılı/puanlanmış bir ajanın o anki durumu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub agent: Agent,
    pub best_score: f64,
    pub generations_survived: u32,
}

/// `data/archive.json` üzerine yazan basit JSON arşivi.
#[derive(Debug, Clone)]
pub struct Archive {
    path: std::path::PathBuf,
}

impl Archive {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_location() -> Self {
        Self::new("data/archive.json")
    }

    pub fn load(&self) -> Result<Vec<ArchiveEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&self.path)
            .with_context(|| format!("arşiv okunamadı: {:?}", self.path))?;
        if data.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&data).context("arşiv JSON çözümlenemedi")
    }

    pub fn save(&self, entries: &[ArchiveEntry]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &self.path,
            serde_json::to_string_pretty(entries).expect("entries serialize"),
        )
        .with_context(|| format!("arşive yazılamadı: {:?}", self.path))
    }

    /// Ajanı (varsa güncelleyerek) arşive ekler ve kaydeder.
    pub fn upsert(&self, entry: ArchiveEntry, max_entries: usize) -> Result<()> {
        let mut entries = self.load()?;
        match entries.iter_mut().find(|e| e.agent.id == entry.agent.id) {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        }
        // En yüksek fitness'a sahip max_entries kaydı koru.
        entries.sort_by(|a, b| {
            b.best_score
                .partial_cmp(&a.best_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries.truncate(max_entries);
        self.save(&entries)
    }
}

// ---------------------------------------------------------------------------
// PopulationManager (Adım 3.2)
// ---------------------------------------------------------------------------

/// Popülasyon yönetimi: ebeveyn seçilimi ve çocuk üretimi.
#[derive(Debug, Clone)]
pub struct PopulationManager {
    pub population_size: usize,
    pub elite_count: usize,
}

impl Default for PopulationManager {
    fn default() -> Self {
        Self {
            population_size: 6,
            elite_count: 2,
        }
    }
}

impl PopulationManager {
    pub fn new(population_size: usize, elite_count: usize) -> Self {
        Self {
            population_size: population_size.max(2),
            elite_count: elite_count.min(population_size.saturating_sub(1)).max(1),
        }
    }

    /// Rulet tekerleği: fitness ile orantılı ağırlıklı seçim.
    /// Tüm fitness'lar 0 ise uniform seçim yapılır.
    pub fn select_parents_roulette(&self, agents: &[Agent], count: usize) -> Vec<Uuid> {
        assert!(!agents.is_empty(), "boş popülasyondan ebeveyn seçilemez");
        let total: f64 = agents.iter().map(|a| a.fitness_score.max(0.0)).sum();
        let mut rng = rand::thread_rng();
        let mut picked = Vec::with_capacity(count);
        for round in 0..count {
            if total <= f64::EPSILON {
                // Fitness yok → rastgele uniform.
                let i = rng.gen_range(0..agents.len());
                picked.push(agents[i].id);
            } else {
                let mut roll = rng.gen_range(0.0..total);
                for a in agents {
                    roll -= a.fitness_score.max(0.0);
                    if roll <= 0.0 {
                        picked.push(a.id);
                        break;
                    }
                }
                // Float kayması nedeniyle hiç eşleşmezse son adayı garanti et.
                if picked.len() < round + 1 {
                    let i = rng.gen_range(0..agents.len());
                    picked.push(agents[i].id);
                }
            }
        }
        picked
    }

    /// Turnuva seçilimi: `k` rastgele aday içinden en fit olan.
    pub fn select_parents_tournament(&self, agents: &[Agent], k: usize, count: usize) -> Vec<Uuid> {
        assert!(!agents.is_empty(), "boş popülasyondan ebeveyn seçilemez");
        let mut rng = rand::thread_rng();
        (0..count)
            .map(|_| {
                // `select_unordered` yerine sıralı `seq`: rust_std 1.40 ile uyumlu
                // ve eşit fitness'ta deterministik davranır.
                (0..k.max(2))
                    .map(|_| agents[rng.gen_range(0..agents.len())].clone())
                    .reduce(|best, cand| {
                        if cand.fitness_score > best.fitness_score {
                            cand
                        } else {
                            best
                        }
                    })
                    .expect("turnuva boş olamaz")
                    .id
            })
            .collect()
    }

    /// Elitleri korur, kalanları ebeveynlerden mutant çocuklarla doldurur.
    pub fn next_generation(&self, agents: &[Agent], parents: &[Uuid]) -> Vec<Agent> {
        let mut sorted: Vec<Agent> = agents.to_vec();
        sorted.sort_by(|a, b| {
            b.fitness_score
                .partial_cmp(&a.fitness_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut next: Vec<Agent> = sorted.iter().take(self.elite_count).cloned().collect();
        let by_id: std::collections::HashMap<Uuid, &Agent> =
            sorted.iter().map(|a| (a.id, a)).collect();
        while next.len() < self.population_size {
            let parent = parents
                .iter()
                .filter_map(|id| by_id.get(id).copied())
                .next_back()
                .or(sorted.first())
                .expect("popülasyon boş değil");
            // Basit genom mutasyonu: prompt sonuna keşif notu ekle.
            // `mutate_prompt_with_hint` önceki notları temizlediği için
            // jenerasyonlar arası not birikmesi olmaz.
            let mut mutated = if parent.fitness_score >= 100.0 {
                // Zaten mükemmel olan genomu bozma: prompt'u aynen koru.
                parent.mutate_prompt(parent.system_prompt.clone())
            } else {
                let hint = format!(
                    "[keşif notu g{}] Denemeler arası tutarlı ol; önceki hatalardan ders çıkar.",
                    parent.generation + 1
                );
                parent.mutate_prompt_with_hint(&hint)
            };
            // Çocukları ebeveyn fitness'ıyla başlat ki bir sonraki turnuvada
            // sıfırlanmış skorla elenmesinler.
            mutated.fitness_score = parent.fitness_score;
            next.push(mutated);
        }
        next
    }
}

// ---------------------------------------------------------------------------
// Evolve döngüsü (Adım 2.3 + 3.3 — paralel değerlendirme)
// ---------------------------------------------------------------------------

/// Tek bir ajan × problem çiftinin async değerlendirmesi.
pub async fn evaluate_agent<B: LlmBackend + ?Sized>(
    llm: Arc<B>,
    evaluator: Arc<quine_eval::Evaluator>,
    agent: Agent,
    problem: Arc<Problem>,
    model: String,
) -> Result<(Agent, EvaluationResult)> {
    let request = LlmRequest::new(
        model.clone(),
        agent.system_prompt.clone(),
        problem.to_llm_prompt(),
    );
    let response = llm
        .generate(&request)
        .await
        .context("çözüm üretim isteği")?;
    let code = response.extract_code();
    let result = evaluator.evaluate(agent.id, &problem, &code).await;
    let mut agent = agent;
    agent.fitness_score = result.score;
    Ok((agent, result))
}

/// Tek ajanlı evrim turunun sonucu.
#[derive(Debug, Clone)]
pub struct EvolveStep {
    /// Bir sonraki turda kullanılacak ajan (mutasyon kabul edildiyse çocuk,
    /// aksi halde girdi ajanının aynısı).
    pub agent: Agent,
    /// Bu turda **girdi ajanının** değerlendirme sonucu.
    pub result: EvaluationResult,
    /// Mutasyonun kabul edilip edilmediği (elitizm kapısı).
    pub accepted: bool,
    /// LLM'in ürettiği (değerlendirilen) kod — teşhis için.
    pub code: String,
}

/// Tek ajanlı **elitist tepe-tırmanma** turu (Adım 2.2).
///
/// Girdi ajanının prompt'uyla kod üretir ve değerlendirir; başarısızsa
/// [`MutationEngine::refine_prompt`] ile tek bir kısa ders ekleyip mutasyon
/// üretir. Mutasyon **yalnızca mevcut fitness düşmezse** kabul edilir; aksi
/// halde girdi ajanı (ve fitness'ı) aynen korunur. Bu, döngünün rastgele
/// yürüyüşe dönüp iyi prompt'u bozmasını engeller.
pub async fn evolve_step<B: LlmBackend + ?Sized>(
    llm: &B,
    evaluator: &quine_eval::Evaluator,
    agent: &Agent,
    problem: &Problem,
    model: &str,
    failures: &[EvaluationResult],
) -> Result<EvolveStep> {
    let request = LlmRequest::new(model, agent.system_prompt.clone(), problem.to_llm_prompt());
    let response = llm.generate(&request).await.context("çözüm üretimi")?;
    let code = response.extract_code();
    let result = evaluator.evaluate(agent.id, problem, &code).await;

    if result.success {
        let mut solved = agent.clone();
        solved.fitness_score = result.score;
        return Ok(EvolveStep {
            agent: solved,
            result,
            accepted: true,
            code,
        });
    }

    let engine = MutationEngine::new(llm);
    let mut report = failures.to_vec();
    report.push(result.clone());
    let candidate = match engine.refine_prompt(agent, model, &report).await {
        Ok(prompt) => agent.mutate_prompt(prompt),
        Err(e) => {
            tracing::warn!("refine başarısız ({e:#}); heuristik mutasyon uygulanıyor");
            let heuristic = apply_rule(
                agent,
                "Kenar durumları (0, 1, negatif, boş girdi) kontrol et.",
            );
            agent.mutate_prompt(heuristic)
        }
    };

    let accepted = result.score >= agent.fitness_score;
    let next = if accepted {
        let mut child = candidate;
        child.fitness_score = result.score;
        child
    } else {
        agent.clone()
    };
    Ok(EvolveStep {
        agent: next,
        result,
        accepted,
        code,
    })
}

/// Bir jenerasyonun tüm ajanlarını `tokio::spawn` ile **paralel** değerlendirir.
pub async fn evaluate_population(
    llm: Arc<dyn LlmBackend>,
    evaluator: Arc<quine_eval::Evaluator>,
    population: Vec<Agent>,
    problems: Vec<Problem>,
    model: String,
) -> Vec<(Agent, Vec<EvaluationResult>)> {
    let problems: Vec<Arc<Problem>> = problems.into_iter().map(Arc::new).collect();
    let handles: Vec<_> = population
        .into_iter()
        .map(|agent| {
            let llm = llm.clone();
            let ev = evaluator.clone();
            let problems = problems.clone();
            let model = model.clone();
            tokio::spawn(async move {
                let mut results = Vec::new();
                let mut agent = agent;
                for p in problems {
                    match evaluate_agent(llm.clone(), ev.clone(), agent.clone(), p, model.clone())
                        .await
                    {
                        Ok((a, r)) => {
                            agent = a;
                            results.push(r);
                        }
                        Err(e) => tracing::warn!("değerlendirme hatası: {e:#}"),
                    }
                }
                let avg = if results.is_empty() {
                    0.0
                } else {
                    results.iter().map(|r| r.score).sum::<f64>() / results.len() as f64
                };
                agent.fitness_score = avg;
                (agent, results)
            })
        })
        .collect();

    let mut out = Vec::new();
    for h in handles {
        match h.await {
            Ok(v) => out.push(v),
            Err(e) => tracing::error!("spawn görevi panikledi: {e}"),
        }
    }
    out
}

/// Kod mutasyonu (Faz 4 / Adım 4.3) — kod üretimiyle ilgili hatalar.
#[derive(Debug, thiserror::Error)]
pub enum CodeMutationError {
    #[error("LLM yanıtında uygulanabilir kod bulunamadı")]
    NoCode,
    #[error("guardian reddetti: {0}")]
    Guardian(String),
}

/// LLM'in ürettiği **kaynak kod değişikliği**ni guardian'dan geçirip diske
/// uygulayan motor.
///
/// Akış (güvenlik önce, yazma sonra):
/// 1. LLM'den yalnızca dosyanın **tam yeni içeriğini** üretmesi istenir.
/// 2. İçerik [`DiffAnalyzer`] ile taranır; kritik ihlal varsa **hiçbir şey
///    yazılmaz** ve [`CodeMutationError::Guardian`] döner.
/// 3. Yalnızca temiz içerik atomik olarak hedef dosyaya yazılır; eski içerik
///    `.<ad>.bak` yedeğine alınır.
pub struct CodeMutator<'a, B: LlmBackend + ?Sized> {
    llm: &'a B,
    analyzer: quine_guardian::DiffAnalyzer,
}

impl<'a, B: LlmBackend + ?Sized> CodeMutator<'a, B> {
    pub fn new(llm: &'a B) -> Self {
        Self {
            llm,
            analyzer: quine_guardian::DiffAnalyzer::default(),
        }
    }

    /// Guardian'ı özel kurallarla değiştir (test enjeksiyonu için).
    pub fn with_analyzer(mut self, analyzer: quine_guardian::DiffAnalyzer) -> Self {
        self.analyzer = analyzer;
        self
    }

    /// `instruction` doğrultusunda hedef dosyanın yeni içeriğini üretir.
    pub async fn propose(
        &self,
        model: &str,
        path: &std::path::Path,
        instruction: &str,
    ) -> Result<String> {
        let current = std::fs::read_to_string(path)
            .with_context(|| format!("hedef dosya okunamadı: {}", path.display()))?;
        let system = "Sen bir Rust kaynak-kod mutasyon ajanısın. Yalnızca istenen dosyanın TAM \
                      yeni içeriğini tek bir ```rust kod bloğu içinde üret. Açıklama yazma.";
        let prompt = format!(
            "Dosya: {}\n\nMevcut içerik:\n```rust\n{current}\n```\n\nİstenen değişiklik:\n{instruction}",
            path.display()
        );
        let resp = self
            .llm
            .generate(&LlmRequest::new(model, system, prompt))
            .await
            .context("kod mutasyonu isteği")?;
        let code = resp.extract_code();
        if code.trim().is_empty() {
            return Err(CodeMutationError::NoCode.into());
        }
        Ok(code)
    }

    /// Öneriyi değerlendirir ve temizse uygular; dönen değer hedef dosyadır.
    ///
    /// `Ok(path)` → mutasyon uygulandı; `Err(Guardian)` → engellendi (dosya
    /// değişmedi). Diğer hatalar LLM/IO kaynaklıdır.
    pub async fn apply(
        &self,
        model: &str,
        path: &std::path::Path,
        instruction: &str,
    ) -> Result<std::path::PathBuf> {
        let code = self.propose(model, path, instruction).await?;
        self.analyzer
            .analyze(&code)
            .map_err(|v| CodeMutationError::Guardian(v.to_string()))?;
        self.write_atomic(path, &code)
            .with_context(|| format!("mutasyon yazılamadı: {}", path.display()))?;
        Ok(path.to_path_buf())
    }

    /// Yedeği alıp dosyayı geçici bir ada yazdıktan sonra yerine taşır.
    fn write_atomic(&self, path: &std::path::Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let backup = path.with_extension(format!(
            "{}bak",
            path.extension()
                .map(|e| format!("{}.", e.to_string_lossy()))
                .unwrap_or_default()
        ));
        if path.exists() {
            std::fs::copy(path, &backup)
                .with_context(|| format!("yedek alınamadı: {}", backup.display()))?;
        }
        let tmp = path.with_extension("quine-tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quine_llm::EchoBackend;

    #[test]
    fn roulette_prefers_fitter_agents() {
        let pm = PopulationManager::default();
        let mut weak = Agent::new("weak");
        weak.fitness_score = 1.0;
        let mut strong = Agent::new("strong");
        strong.fitness_score = 99.0;
        let picks = pm.select_parents_roulette(&[weak.clone(), strong.clone()], 200);
        let strong_count = picks.iter().filter(|id| **id == strong.id).count();
        assert!(
            strong_count > 150,
            "rulet fitness'ı takip etmeli: {strong_count}/200"
        );
    }

    #[test]
    fn tournament_picks_best_of_k() {
        let pm = PopulationManager::default();
        let mut a = Agent::new("a");
        a.fitness_score = 10.0;
        let mut b = Agent::new("b");
        b.fitness_score = 90.0;
        let picks = pm.select_parents_tournament(&[a.clone(), b.clone()], 8, 50);
        // Popülasyon 2 kişilik ve fitness farkı büyük (90 vs 10): k=8 çekişte
        // a'nın hiç çekilmeme olasılığı (1/2)^8 = 1/256 — pratikte tüm seçimler b olmalı.
        // Yine de nadir randomness'e karşı toleranslı doğrulama: >= %90 b seçilmeli.
        let b_count = picks.iter().filter(|id| **id == b.id).count();
        assert!(
            b_count * 10 >= picks.len() * 9,
            "turnuva en fit'i secmedi: {b_count}/{}",
            picks.len()
        );
    }

    #[test]
    fn next_generation_keeps_elites_and_grows_children() {
        let pm = PopulationManager::new(6, 2);
        let mut agents = Vec::new();
        for i in 0..4 {
            let mut a = Agent::new(format!("a{i}"));
            a.fitness_score = i as f64;
            agents.push(a);
        }
        let parents: Vec<Uuid> = agents.iter().map(|a| a.id).collect();
        let next = pm.next_generation(&agents, &parents);
        assert_eq!(next.len(), 6);
        // En iyi 2 elit korunmalı.
        assert!(next.iter().any(|a| a.fitness_score == 3.0));
        assert!(next.iter().any(|a| a.fitness_score == 2.0));
        // Çocuklar jenerasyon 1.
        assert!(next.iter().any(|a| a.generation == 1));
    }

    #[test]
    fn archive_roundtrip_and_upsert() {
        let dir = std::env::temp_dir().join(format!("quine-archive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.json");
        std::fs::remove_file(&path).ok();
        let ar = Archive::new(&path);
        assert!(ar.load().unwrap().is_empty());
        let mut a = Agent::new("arch-1");
        a.fitness_score = 42.0;
        ar.upsert(
            ArchiveEntry {
                agent: a.clone(),
                best_score: 42.0,
                generations_survived: 1,
            },
            10,
        )
        .unwrap();
        let loaded = ar.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].agent.id, a.id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn mutation_engine_returns_new_prompt_from_echo_backend() {
        let llm = EchoBackend::new("Yeni sistem prompt'u: kenar durumları kontrol et.");
        let engine = MutationEngine::new(&llm);
        let agent = Agent::new("evolve-me");
        let r = EvaluationResult::perfect("fib-001", agent.id, 1);
        let mut r = r;
        r.success = false;
        r.stderr = "expected 55 got 89".into();
        let new_prompt = engine
            .refine_prompt(&agent, "fake-model", &[r])
            .await
            .unwrap();
        assert!(new_prompt.contains("kenar durumları"));
        assert!(!new_prompt.contains("```"));
    }

    #[test]
    fn extract_rule_skips_fences_code_and_returns_prose() {
        // Model kuralı ```rust fence'ine sarınca eski kod "rust"u kural sanıyordu.
        assert_eq!(
            extract_rule("```rust\nNegatif n için false döndür.\n```").as_deref(),
            Some("Negatif n için false döndür.")
        );
        assert_eq!(
            extract_rule("```\n  Kenar durumları kontrol et.  \n```").as_deref(),
            Some("Kenar durumları kontrol et.")
        );
        // Kod satırı kural olarak kabul edilmemeli (regresyon: `[ders] fn ...`).
        assert_eq!(
            extract_rule("```rust\nfn factorial(n: u32) -> u64 { todo!() }\n```"),
            None
        );
        assert_eq!(extract_rule("```rust"), None);
        assert_eq!(extract_rule(""), None);
    }

    #[test]
    fn extract_rule_truncates_long_prose_at_sentence() {
        let long = "Rust'ta u32 ve u64 arasında doğrudan çarpma yapılamaz. Bu yüzden \
                    her ara sonucu u64'e çevir ve fold ile çarp.";
        let rule = extract_rule(long).unwrap();
        assert_eq!(
            rule,
            "Rust'ta u32 ve u64 arasında doğrudan çarpma yapılamaz."
        );
        assert!(rule.chars().count() <= 160);
    }

    #[test]
    fn apply_rule_does_not_duplicate_same_rule() {
        // Heuristik yol her jenerasyonda aynı kuralı ekliyordu; tekrar etmemeli.
        let root = Agent::new("root");
        let p1 = apply_rule(&root, "Kenar durumları kontrol et.");
        let agent1 = root.mutate_prompt(p1);
        let p2 = apply_rule(&agent1, "Kenar durumları kontrol et.");
        assert_eq!(
            p2.lines().filter(|l| l.starts_with("[ders]")).count(),
            1,
            "aynı kural yinelenmemeli: {p2}"
        );
    }

    fn temp_target(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("quine-codemut-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn code_mutator_applies_clean_change_atomically() {
        let path = temp_target("tool.rs", "pub fn greet() -> &'static str { \"v1\" }\n");
        let llm =
            quine_llm::EchoBackend::new("```rust\npub fn greet() -> &'static str { \"v2\" }\n```");
        let mutator = CodeMutator::new(&llm);
        mutator
            .apply("echo", &path, "selamlamayı v2 yap")
            .await
            .expect("temiz mutasyon uygulanmalı");

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("v2"));
        let backup = path.with_extension("rs.bak");
        assert!(backup.exists(), "eski içerik yedeklenmeli");
        assert!(std::fs::read_to_string(&backup).unwrap().contains("v1"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn code_mutator_blocks_destructive_change_and_leaves_file_intact() {
        // Faz 4 kabul kriteri: `rm -rf /` içeren üretim engellenmeli ve dosya
        // dokunulmadan kalmalı.
        let original = "pub fn safe() -> u32 { 1 }\n";
        let path = temp_target("evil.rs", original);
        let llm = quine_llm::EchoBackend::new(
            "```rust\nfn main() { std::process::Command::new(\"sh\").arg(\"-c\").arg(\"rm -rf /\").status().unwrap(); }\n```",
        );
        let mutator = CodeMutator::new(&llm);
        let err = mutator
            .apply("echo", &path, "kötü niyetli")
            .await
            .expect_err("guardian engellemeli");
        assert!(matches!(
            err.downcast_ref::<CodeMutationError>(),
            Some(CodeMutationError::Guardian(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(
            !path.with_extension("rs.bak").exists(),
            "engellenince yedek de oluşmamalı"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn refine_prompt_preserves_base_and_accumulates_rules() {
        let llm = EchoBackend::new("Taşma olmaması için u64 kullan.");
        let engine = MutationEngine::new(&llm);
        let agent = Agent::new("birikim");
        let mut r = EvaluationResult::perfect("ext-factorial", agent.id, 1);
        r.success = false;
        r.stderr = "error[E0277]: u32 -> u64".into();

        let first = engine
            .refine_prompt(&agent, "m", &[r.clone()])
            .await
            .unwrap();
        // Taban prompt korunmalı (küçük modelin çökertmesi engellenir).
        assert!(first.contains("deneyimli bir Rust geliştiricisisin"));
        assert!(first.contains("[ders] Taşma olmaması"));

        // İkinci tur: yeni ders eklenir, eskisi kaybolmaz.
        let agent2 = agent.mutate_prompt(first);
        let llm2 = EchoBackend::new("Sonucu açıkça döndür, gereksiz döngü kurma.");
        let engine2 = MutationEngine::new(&llm2);
        let second = engine2.refine_prompt(&agent2, "m", &[r]).await.unwrap();
        assert!(second.contains("[ders] Taşma olmaması"));
        assert!(second.contains("[ders] Sonucu açıkça döndür"));
        assert!(second.contains("deneyimli bir Rust geliştiricisisin"));
    }

    #[tokio::test]
    async fn parallel_population_evaluation_completes() {
        // Echo backend + local sandbox ile mini popülasyon turu (offline çalışır).
        use quine_eval::{Evaluator, LocalProcessSandbox};

        let llm = Arc::new(EchoBackend::fibonacci_solver());
        let ev = Arc::new(Evaluator::new(Box::new(LocalProcessSandbox::default())));
        let pop = vec![Agent::new("p1"), Agent::new("p2")];
        let fib = quine_bench_simple::fibonacci();
        let scored = evaluate_population(llm, ev, pop, vec![fib], "fake".into()).await;
        assert_eq!(scored.len(), 2);
        for (agent, results) in &scored {
            assert_eq!(results.len(), 1);
            assert_eq!(agent.fitness_score, results[0].score);
        }
        // Fibonacci echo çözümü fib problemini geçmeli.
        assert!(scored.iter().any(|(_, rs)| rs[0].success));
    }

    #[test]
    fn next_generation_does_not_accumulate_hints() {
        // 4 jenerasyon ilerlet; her jenerasyonda prompt'ta tek bir not olmalı.
        let pm = PopulationManager::new(4, 1);
        let mut agents: Vec<Agent> = (0..4).map(|i| Agent::new(format!("a{i}"))).collect();
        agents[0].fitness_score = 100.0; // elit + mükemmel
        agents[1].fitness_score = 50.0;
        agents[2].fitness_score = 30.0;
        agents[3].fitness_score = 10.0;

        for _ in 0..4 {
            let parents = pm.select_parents_tournament(&agents, 3, 4);
            agents = pm.next_generation(&agents, &parents);
            for a in &agents {
                let hints = a
                    .system_prompt
                    .lines()
                    .filter(|l| l.starts_with("[keşif notu g"))
                    .count();
                assert!(hints <= 1, "not birikti ({}): {}", hints, a.system_prompt);
            }
        }
    }
}
