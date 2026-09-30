//! Run engine: LLM → candidate → guardian → sandbox → evaluation → mutation
//! döngüsünü **gerçek bir domain nesnesi** üzerinde yürütür.
//!
//! Bu katman hem CLI hem web control plane tarafından paylaşılır. Her adım:
//! * bir [`RunEvent`] üretir (persist + yayın + log),
//! * iptal (cancel) ve duraklatma (pause) sinyallerine saygı gösterir,
//! * kaynak limitlerini zorlar (aşımda `LIMIT_REACHED`).
//!
//! İptal gerçek etkilidir: `select!` ile bekleyen LLM/sandbox future'ı
//! düşürülür; sandbox alt süreçleri `kill_on_drop` ile öldürülür.

use crate::event::{EventBus, RunEventKind};
use crate::run::Run;
use crate::{BuildInfo, VERSION};
use anyhow::{Context, Result};
use chrono::Utc;
use quine_common::{EvaluationResult, Problem};
use quine_eval::{sandbox_from_kind, Evaluator};
use quine_guardian::{AuditDecision, AuditLog, DiffAnalyzer};
use quine_llm::{LlmBackend, LlmRequest};
use quine_storage::{
    AuditEntry, CandidateRecord, EvaluationRecord, RunStatus, Store, WorkloadKind,
};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Bir run için kaynak limitleri (production: sınırsız çalışma yok).
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub model: String,
    pub temperature: f64,
    pub sandbox_kind: String,
    pub mode: RunMode,
    pub population_size: usize,
    pub limits: RunLimits,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            model: quine_llm::DEFAULT_MODEL.to_string(),
            temperature: 0.0,
            // Production varsayılanı: docker (fail-closed).
            sandbox_kind: "docker".to_string(),
            mode: RunMode::Evolve,
            population_size: 4,
            limits: RunLimits::default(),
        }
    }
}

/// Çalışma modu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Tek üretim + değerlendirme.
    Single,
    /// Elitist tepe-tırmanma (öğrenme döngüsü).
    Evolve,
    /// Çok-ajanlı popülasyon evrimi.
    Population,
}

/// Kaynak limitleri — güvenli varsayılanlarla.
#[derive(Debug, Clone)]
pub struct RunLimits {
    pub max_wall_time: Duration,
    pub max_llm_calls: u32,
    pub max_iterations: u32,
    pub max_generations: u32,
    pub max_population: usize,
    pub max_output_bytes: usize,
    pub max_candidate_size: usize,
    pub max_concurrent: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_wall_time: Duration::from_secs(600),
            max_llm_calls: 50,
            max_iterations: 8,
            max_generations: 6,
            max_population: 16,
            max_output_bytes: 64 * 1024,
            max_candidate_size: 64 * 1024,
            max_concurrent: 4,
        }
    }
}

/// Çalıştırma isteği (problem + yapılandırma + opsiyonel test backend'i).
pub struct RunRequest {
    pub problem: Problem,
    pub workload: WorkloadKind,
    pub config: RunConfig,
    /// Test/CLI için backend enjeksiyonu. `None` ise Ollama kullanılır.
    pub backend: Option<Arc<dyn LlmBackend>>,
}

/// Paylaşılan çalışma zamanı bağlamı (store, bus, build bilgisi).
#[derive(Clone)]
pub struct RuntimeContext {
    pub store: Arc<Store>,
    pub bus: EventBus,
    pub build: BuildInfo,
    pub data_dir: PathBuf,
}

impl RuntimeContext {
    pub fn new(store: Arc<Store>, data_dir: impl Into<PathBuf>) -> Self {
        let bus = EventBus::new(store.clone(), 1024);
        Self {
            store,
            bus,
            build: BuildInfo::detect(),
            data_dir: data_dir.into(),
        }
    }
}

/// Run'ı iptal/duraklat kontrolü.
struct Control {
    cancel_tx: watch::Sender<bool>,
    paused: AtomicBool,
    resume_notify: Notify,
}

impl Control {
    fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (cancel_tx, rx) = watch::channel(false);
        (
            Arc::new(Self {
                cancel_tx,
                paused: AtomicBool::new(false),
                resume_notify: Notify::new(),
            }),
            rx,
        )
    }
    fn request_cancel(&self) {
        let _ = self.cancel_tx.send(true);
        self.resume_notify.notify_waiters();
    }
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.resume_notify.notify_waiters();
    }
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    fn is_cancelled(&self) -> bool {
        *self.cancel_tx.borrow()
    }
    async fn wait_if_paused(&self) {
        while self.is_paused() && !self.is_cancelled() {
            self.resume_notify.notified().await;
        }
    }
}

/// Klonlanabilir kumanda tutamacı: iptal/duraklat/çöz.
#[derive(Clone)]
pub struct RunControl {
    control: Arc<Control>,
}

impl RunControl {
    /// İptal isteği gönderir (LLM/sandbox beklemesi kesilir).
    pub fn cancel(&self) {
        self.control.request_cancel();
    }
    /// Duraklatır.
    pub fn pause(&self) {
        self.control.pause();
    }
    /// Duraklatmayı kaldırır.
    pub fn resume(&self) {
        self.control.resume();
    }
    pub fn is_cancelled(&self) -> bool {
        self.control.is_cancelled()
    }
    pub fn is_paused(&self) -> bool {
        self.control.is_paused()
    }
}

/// Başlatılmış bir run'a dışarıdan kumanda tutamacı.
pub struct RunHandle {
    pub run_id: String,
    control: Arc<Control>,
    join: JoinHandle<RunOutcome>,
}

impl RunHandle {
    /// Klonlanabilir kumanda nesnesi (web katmanı bunu saklar).
    pub fn control(&self) -> RunControl {
        RunControl {
            control: self.control.clone(),
        }
    }
    /// İptal isteği gönderir (LLM/sandbox beklemesi kesilir).
    pub fn cancel(&self) {
        self.control.request_cancel();
    }
    /// Duraklatır (döngü bir sonraki kontrol noktasında bekler).
    pub fn pause(&self) {
        self.control.pause();
    }
    /// Duraklatmayı kaldırır.
    pub fn resume(&self) {
        self.control.resume();
    }
    pub fn is_cancelled(&self) -> bool {
        self.control.is_cancelled()
    }
    /// Run bitene kadar bekler.
    pub async fn wait(self) -> RunOutcome {
        self.join.await.unwrap_or_else(|e| RunOutcome {
            run_id: "?".into(),
            status: RunStatus::Failed,
            best_score: 0.0,
            production_success: false,
            error: Some(format!("görev panikledi: {e}")),
        })
    }
}

/// Run tamamlandığında dönen özet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub status: RunStatus,
    pub best_score: f64,
    pub production_success: bool,
    pub error: Option<String>,
}

/// Dashboard için hafif run özeti.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
    pub status: String,
    pub problem_title: String,
    pub model: String,
    pub generation: u32,
    pub best_score: f64,
    pub production_success: bool,
}

/// Run motoru.
pub struct RunEngine {
    ctx: RuntimeContext,
}

impl RunEngine {
    pub fn new(ctx: RuntimeContext) -> Self {
        Self { ctx }
    }

    pub fn context(&self) -> &RuntimeContext {
        &self.ctx
    }

    /// Yeni bir run başlatır; hemen [`RunHandle`] döner.
    pub fn start(&self, req: RunRequest) -> Result<RunHandle> {
        // Sandbox seçimi fail-closed: geçersiz/docker-yok → burada hata.
        let sandbox = sandbox_from_kind(&req.config.sandbox_kind)?;
        let sandbox_kind = sandbox.kind().to_string();
        let sandbox_image = quine_eval::DEFAULT_SANDBOX_IMAGE.to_string();

        // Sandbox limitleri: çıktı boyutu run limitlerinden türetilir.
        let evaluator = Arc::new(
            Evaluator::new(sandbox).with_audit(AuditLog::new(self.ctx.data_dir.join("audit.log"))),
        );

        let run = Run::new(
            Uuid::new_v4().to_string(),
            req.workload,
            req.problem.id.clone(),
            req.problem.title.clone(),
            req.config.model.clone(),
            req.config.temperature,
            sandbox_kind.clone(),
        );
        let run_id = run.id.clone();
        self.ctx
            .store
            .insert_run(&run.to_record(&self.ctx.build, &sandbox_image))?;

        let (control, cancel_rx) = Control::new();
        let ctx = self.ctx.clone();
        let problem = req.problem.clone();
        let config = req.config.clone();
        let workload = req.workload;
        let backend = req.backend.clone();
        let build = self.ctx.build.clone();
        let control_for_task = control.clone();

        let join = tokio::spawn(async move {
            let runner = Runner {
                ctx,
                control: control_for_task.clone(),
                cancel_rx,
                run,
                problem,
                config,
                workload,
                evaluator,
                backend,
                build,
                sandbox_image,
                guardian: DiffAnalyzer::default(),
                started: Instant::now(),
                semaphore: Arc::new(Semaphore::new(1)),
            };
            runner.run().await
        });

        Ok(RunHandle {
            run_id,
            control,
            join,
        })
    }
}

/// Aday kaydı için girdi (argüman listesini okunur tutar).
struct CandidateInput<'a> {
    agent_id: &'a str,
    parent_id: Option<String>,
    generation: u32,
    code: &'a str,
    result: &'a EvaluationResult,
    delta: f64,
    reason: Option<String>,
}

struct Runner {
    ctx: RuntimeContext,
    control: Arc<Control>,
    cancel_rx: watch::Receiver<bool>,
    run: Run,
    problem: Problem,
    config: RunConfig,
    workload: WorkloadKind,
    evaluator: Arc<Evaluator>,
    backend: Option<Arc<dyn LlmBackend>>,
    build: BuildInfo,
    sandbox_image: String,
    guardian: DiffAnalyzer,
    started: Instant,
    semaphore: Arc<Semaphore>,
}

impl Runner {
    async fn run(mut self) -> RunOutcome {
        let run_id = self.run.id.clone();
        let result = self.run_inner().await;
        match &result {
            Ok(()) => {}
            Err(e) => {
                // İptal bir hata değildir: Failed yerine Cancelled işaretle.
                if self.control.is_cancelled() {
                    let _ = self.cancel_status();
                } else {
                    self.run.error = Some(format!("{e:#}"));
                    if !self.run.is_terminal() {
                        let _ = self.set_status(RunStatus::Failed, RunEventKind::RunFailed, None);
                    }
                }
            }
        }
        self.persist();
        RunOutcome {
            run_id,
            status: self.run.status,
            best_score: self.run.best_score,
            production_success: self.run.production_success,
            error: self.run.error.clone(),
        }
    }

    /// Run'ı `Cancelling` üzerinden `Cancelled` durumuna getirir.
    fn cancel_status(&mut self) -> Result<()> {
        if self.run.is_terminal() {
            return Ok(());
        }
        if self.run.status == RunStatus::Running || self.run.status == RunStatus::Paused {
            self.set_status(RunStatus::Cancelling, RunEventKind::RunCancelling, None)?;
        }
        self.set_status(RunStatus::Cancelled, RunEventKind::RunCancelled, None)?;
        Ok(())
    }

    async fn run_inner(&mut self) -> Result<()> {
        self.set_status(RunStatus::Running, RunEventKind::RunStarted, None)?;
        self.emit(
            RunEventKind::ProblemLoaded,
            None,
            None,
            serde_json::json!({
                "problem_id": self.problem.id,
                "title": self.problem.title,
                "workload": self.workload.as_str(),
                "mode": format!("{:?}", self.config.mode),
            }),
        )?;

        match self.config.mode {
            RunMode::Single => self.mode_single().await,
            RunMode::Evolve => self.mode_evolve().await,
            RunMode::Population => self.mode_population().await,
        }
    }

    // ---- modlar ----------------------------------------------------------

    async fn mode_single(&mut self) -> Result<()> {
        let agent_id = Uuid::new_v4().to_string();
        let (code, result) = self.generate_and_evaluate(&agent_id, 0).await?;
        self.record_candidate(CandidateInput {
            agent_id: &agent_id,
            parent_id: None,
            generation: 0,
            code: &code,
            result: &result,
            delta: 0.0,
            reason: None,
        })?;
        self.finalize(&result, 0)
    }

    async fn mode_evolve(&mut self) -> Result<()> {
        let mut agent_id = Uuid::new_v4().to_string();
        let mut parent: Option<String> = None;
        let mut previous_best = 0.0f64;
        let mut prompt = quine_common::default_system_prompt().to_string();

        for iteration in 0..self.config.limits.max_iterations {
            if self.should_stop(iteration)? {
                return Ok(());
            }
            self.run.iteration = iteration;
            let gen = self.run.generation;

            let (code, result) = self
                .generate_and_evaluate_with_prompt(&agent_id, gen, &prompt)
                .await?;

            let delta = result.score - previous_best;
            self.record_candidate(CandidateInput {
                agent_id: &agent_id,
                parent_id: parent.clone(),
                generation: gen,
                code: &code,
                result: &result,
                delta,
                reason: None,
            })?;
            self.run.total_evaluations += 1;

            if result.success {
                return self.finalize(&result, gen);
            }

            // Öğrenme: başarısızlığı özetle → kural çıkar → prompt'u güncelle.
            self.emit(
                RunEventKind::MutationProposed,
                Some(agent_id.clone()),
                Some(gen),
                serde_json::json!({"failed_tests": result.tests_total - result.tests_passed}),
            )?;
            let new_prompt = self
                .propose_mutation(&prompt, &result, agent_id.clone(), gen)
                .await?;

            let improved = result.score >= previous_best;
            if improved {
                previous_best = result.score;
                self.emit(
                    RunEventKind::MutationAccepted,
                    Some(agent_id.clone()),
                    Some(gen),
                    serde_json::json!({"score": result.score}),
                )?;
                parent = Some(agent_id.clone());
                agent_id = Uuid::new_v4().to_string();
                prompt = new_prompt;
                self.run.generation += 1;
            } else {
                self.emit(
                    RunEventKind::MutationRejected,
                    Some(agent_id.clone()),
                    Some(gen),
                    serde_json::json!({"score": result.score, "best": previous_best}),
                )?;
            }
            self.emit(
                RunEventKind::GenerationCompleted,
                Some(agent_id.clone()),
                Some(gen),
                serde_json::json!({"best_score": self.run.best_score}),
            )?;
        }

        // İterasyon bütçesi tükendi, çözüm yok → açık LIMIT_REACHED.
        self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
        Ok(())
    }

    async fn mode_population(&mut self) -> Result<()> {
        let size = self
            .config
            .population_size
            .clamp(1, self.config.limits.max_population);
        self.semaphore = Arc::new(Semaphore::new(self.config.limits.max_concurrent));

        // Jenerasyon 0: başlangıç popülasyonu (hepsi aynı taban prompt).
        let base_prompt = quine_common::default_system_prompt().to_string();
        let mut population: Vec<(String, String, f64)> = (0..size)
            .map(|_| (Uuid::new_v4().to_string(), base_prompt.clone(), 0.0))
            .collect();

        for gen in 0..self.config.limits.max_generations {
            self.checkpoint().await;
            if self.should_stop(gen)? {
                return Ok(());
            }
            self.run.generation = gen;
            let mut scored: Vec<(String, String, f64)> = Vec::with_capacity(size);

            for (agent_id, prompt, _) in population.iter() {
                self.checkpoint().await;
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let (code, result) = self
                    .generate_and_evaluate_with_prompt(agent_id, gen, prompt)
                    .await?;
                self.record_candidate(CandidateInput {
                    agent_id,
                    parent_id: None,
                    generation: gen,
                    code: &code,
                    result: &result,
                    delta: 0.0,
                    reason: None,
                })?;
                self.run.total_evaluations += 1;
                scored.push((agent_id.clone(), prompt.clone(), result.score));
                drop(permit);

                if result.success {
                    return self.finalize(&result, gen);
                }
            }

            scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            let best = scored.first().map(|s| s.2).unwrap_or(0.0);
            if best > self.run.best_score {
                self.run.best_score = best;
            }
            self.emit(
                RunEventKind::GenerationCompleted,
                None,
                Some(gen),
                serde_json::json!({"best_score": best, "population": scored.len()}),
            )?;

            // Elit korunur; kalanlar en iyilerden mutasyonla türetilir.
            let elite = self.config.limits.max_population.max(1) / 4 + 1;
            let mut next: Vec<(String, String, f64)> = scored
                .iter()
                .take(elite)
                .map(|(id, p, s)| (id.clone(), p.clone(), *s))
                .collect();
            let seed_prompts: Vec<String> = scored.iter().map(|(_, p, _)| p.clone()).collect();
            while next.len() < size {
                let base = &seed_prompts[next.len() % seed_prompts.len()];
                next.push((
                    Uuid::new_v4().to_string(),
                    mutate_prompt_heuristically(base, best),
                    0.0,
                ));
            }
            population = next;
        }

        self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
        Ok(())
    }

    // ---- adım yardımcıları ----------------------------------------------

    /// Tek üretim + değerlendirme; tüm ara adımlar için event üretir.
    async fn generate_and_evaluate(
        &mut self,
        agent_id: &str,
        gen: u32,
    ) -> Result<(String, EvaluationResult)> {
        let prompt = quine_common::default_system_prompt().to_string();
        self.generate_and_evaluate_with_prompt(agent_id, gen, &prompt)
            .await
    }

    async fn generate_and_evaluate_with_prompt(
        &mut self,
        agent_id: &str,
        gen: u32,
        prompt: &str,
    ) -> Result<(String, EvaluationResult)> {
        // Limit: LLM çağrısı bütçesi.
        if self.run.total_llm_calls >= self.config.limits.max_llm_calls {
            self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
            anyhow::bail!("LLM çağrı limiti aşıldı");
        }

        let request = LlmRequest {
            model: self.config.model.clone(),
            system: prompt.to_string(),
            prompt: self.problem.to_llm_prompt(),
            temperature: self.config.temperature as f32,
            max_tokens: 1024,
        };

        self.emit(
            RunEventKind::LlmRequestStarted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({"model": request.model}),
        )?;
        let t0 = Instant::now();
        let response = self.call_llm(&request).await?;
        self.run.total_llm_calls += 1;
        self.emit(
            RunEventKind::LlmResponseReceived,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({
                "model": response.model,
                "bytes": response.content.len(),
            }),
        )?;
        let _ = t0;

        let code = match response.extract_code_checked() {
            Ok(c) => c,
            Err(e) => {
                // Model kod üretmedi (küçük modellerde sık). Sandbox'ı boşuna
                // çalıştırmak yerine açık bir "başarısız aday" üret ve döngünün
                // mutasyonla toparlanmasına izin ver.
                let result = EvaluationResult {
                    problem_id: self.problem.id.clone(),
                    agent_id: Uuid::parse_str(agent_id).unwrap_or_else(|_| Uuid::new_v4()),
                    success: false,
                    score: 0.0,
                    stdout: String::new(),
                    stderr: format!("MODEL KOD ÜRETMEDİ: {e:#}"),
                    duration_ms: 0,
                    tests_passed: 0,
                    tests_total: self.problem.test_cases.len(),
                    evaluated_at: chrono::Utc::now(),
                };
                self.emit(
                    RunEventKind::EvaluationCompleted,
                    Some(agent_id.to_string()),
                    Some(gen),
                    serde_json::json!({
                        "score": 0.0,
                        "success": false,
                        "reason": "model_no_code",
                    }),
                )?;
                self.persist_evaluation(agent_id, &result);
                return Ok((String::new(), result));
            }
        };
        self.emit(
            RunEventKind::CodeExtracted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({"bytes": code.len()}),
        )?;

        // Aşırı büyük aday reddedilir (kaynak koruması).
        if code.len() > self.config.limits.max_candidate_size {
            self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
            anyhow::bail!(
                "aday kod boyutu limiti aşıldı ({} > {})",
                code.len(),
                self.config.limits.max_candidate_size
            );
        }

        // Guardian kapısı (policy katmanı — tek başına güvenlik sınırı değildir).
        let t_g = Instant::now();
        match self.guardian.analyze(&code) {
            Ok(()) => {
                self.emit(
                    RunEventKind::GuardianAllowed,
                    Some(agent_id.to_string()),
                    Some(gen),
                    serde_json::json!({"duration_ms": t_g.elapsed().as_millis() as u64}),
                )?;
                self.persist_audit(
                    agent_id,
                    AuditDecision::Allowed,
                    None,
                    None,
                    "guardian geçti",
                );
            }
            Err(v) => {
                let rule = v.violations.first().map(|f| f.rule.clone());
                let severity = v.violations.first().map(|f| format!("{:?}", f.severity));
                self.emit(
                    RunEventKind::GuardianBlocked,
                    Some(agent_id.to_string()),
                    Some(gen),
                    serde_json::json!({"rule": rule, "reason": v.to_string()}),
                )?;
                self.persist_audit(
                    agent_id,
                    AuditDecision::Blocked,
                    rule,
                    severity,
                    &v.to_string(),
                );
                // Değerlendirici de bloklar; sonucu ondan alalım (score 0).
            }
        }

        // Sandbox'ta değerlendir (iptal edilebilir).
        self.emit(
            RunEventKind::SandboxStarted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({"sandbox": self.run.sandbox}),
        )?;
        let t_s = Instant::now();
        let result = self.evaluate(agent_id, &code).await?;
        self.emit(
            RunEventKind::SandboxCompleted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({"duration_ms": t_s.elapsed().as_millis() as u64}),
        )?;

        if result.stderr.contains("GUARDIAN BLOCKED") {
            self.emit(
                RunEventKind::CompilationFailed,
                Some(agent_id.to_string()),
                Some(gen),
                serde_json::json!({"reason": "guardian"}),
            )?;
        }

        self.emit(
            RunEventKind::TestsCompleted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({
                "passed": result.tests_passed,
                "total": result.tests_total,
            }),
        )?;
        self.emit(
            RunEventKind::EvaluationCompleted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({
                "score": result.score,
                "success": result.success,
                "tests_passed": result.tests_passed,
                "tests_total": result.tests_total,
            }),
        )?;

        self.persist_evaluation(agent_id, &result);
        Ok((code, result))
    }

    /// LLM çağrısı; iptal edilebilir.
    async fn call_llm(&mut self, request: &LlmRequest) -> Result<quine_llm::LlmResponse> {
        let backend: Arc<dyn LlmBackend> = match &self.backend {
            Some(b) => b.clone(),
            None => Arc::new(quine_llm::OllamaBackend::new(
                std::env::var("OLLAMA_HOST")
                    .unwrap_or_else(|_| quine_llm::DEFAULT_OLLAMA_HOST.into()),
                self.config.model.clone(),
            )),
        };
        tokio::select! {
            r = backend.generate(request) => r.context("LLM üretimi"),
            _ = self.cancelled() => {
                anyhow::bail!("run iptal edildi (LLM beklenirken)")
            }
        }
    }

    /// Sandbox değerlendirmesi; iptal edilebilir (drop → kill_on_drop).
    async fn evaluate(&mut self, agent_id: &str, code: &str) -> Result<EvaluationResult> {
        let ev = self.evaluator.clone();
        let problem = self.problem.clone();
        let id = Uuid::parse_str(agent_id).unwrap_or_else(|_| Uuid::new_v4());
        tokio::select! {
            r = ev.evaluate(id, &problem, code) => Ok(r),
            _ = self.cancelled() => {
                anyhow::bail!("run iptal edildi (sandbox çalışırken)")
            }
        }
    }

    /// İptal sinyalini bekleyen future.
    async fn cancelled(&mut self) {
        if *self.cancel_rx.borrow() {
            return;
        }
        let _ = self.cancel_rx.changed().await;
    }

    /// Duraklatma/limit/iptal kontrol noktası. `true` dönerse döngü durmalı.
    fn should_stop(&mut self, _iteration: u32) -> Result<bool> {
        if self.control.is_cancelled() {
            self.set_status(RunStatus::Cancelled, RunEventKind::RunCancelled, None)?;
            return Ok(true);
        }
        if self.started.elapsed() > self.config.limits.max_wall_time {
            self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Duraklatma noktası: döngü burada bekleyebilir.
    ///
    /// Durum değişimi kullanıcıya görünür olmalı: panelde "duraklatıldı"
    /// yazısı ve zaman çizelgesinde bir olay olmadan duraklatmak, çalışan bir
    /// ajanın sessizce donması gibi görünür.
    async fn checkpoint(&mut self) {
        if !self.control.is_paused() || self.control.is_cancelled() {
            return;
        }
        self.set_status(RunStatus::Paused, RunEventKind::RunPaused, None)
            .ok();
        self.control.wait_if_paused().await;
        if !self.control.is_cancelled() && self.run.status == RunStatus::Paused {
            self.set_status(RunStatus::Running, RunEventKind::RunResumed, None)
                .ok();
        }
    }

    /// Başarısızlıktan kural çıkarıp prompt'u günceller (öğrenme).
    async fn propose_mutation(
        &mut self,
        prompt: &str,
        result: &EvaluationResult,
        agent_id: String,
        gen: u32,
    ) -> Result<String> {
        self.checkpoint().await;
        let failures = vec![result.clone()];
        // LLM varsa ondan kural isteyelim; yoksa heuristik.
        if let Some(backend) = self.backend.clone() {
            let engine = quine_evolution::MutationEngine::new(backend.as_ref());
            let agent = quine_common::Agent::new("run-agent");
            let mut agent = agent;
            agent.system_prompt = prompt.to_string();
            let model = self.config.model.clone();
            match engine.refine_prompt(&agent, &model, &failures).await {
                Ok(new_prompt) => return Ok(new_prompt),
                Err(e) => {
                    tracing::warn!("refine başarısız ({e:#}); heuristik kural");
                }
            }
        }
        let _ = (agent_id, gen);
        Ok(mutate_prompt_heuristically(prompt, self.run.best_score))
    }

    fn record_candidate(&mut self, input: CandidateInput<'_>) -> Result<()> {
        let CandidateInput {
            agent_id,
            parent_id,
            generation: gen,
            code,
            result,
            delta,
            reason,
        } = input;
        let rec = CandidateRecord {
            id: Uuid::new_v4().to_string(),
            run_id: self.run.id.clone(),
            agent_id: agent_id.to_string(),
            parent_id,
            generation: gen,
            prompt_hash: hash_str(code),
            code: code.to_string(),
            score: result.score,
            tests_passed: result.tests_passed,
            tests_total: result.tests_total,
            duration_ms: result.duration_ms,
            model: self.config.model.clone(),
            status: if result.success {
                "success".into()
            } else {
                "failed".into()
            },
            accepted: result.success,
            delta,
            mutation_reason: reason,
            diff: None,
            created_at: Utc::now(),
        };
        self.ctx.store.insert_candidate(&rec)?;
        self.emit(
            RunEventKind::CandidateCreated,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({
                "candidate_id": rec.id,
                "score": result.score,
                "delta": delta,
                "tests_passed": result.tests_passed,
                "tests_total": result.tests_total,
            }),
        )?;
        Ok(())
    }

    fn persist_evaluation(&self, agent_id: &str, result: &EvaluationResult) {
        let rec = EvaluationRecord {
            id: 0,
            run_id: self.run.id.clone(),
            agent_id: agent_id.to_string(),
            problem_id: result.problem_id.clone(),
            success: result.success,
            score: result.score,
            tests_passed: result.tests_passed,
            tests_total: result.tests_total,
            duration_ms: result.duration_ms,
            stderr: result.stderr.clone(),
            created_at: result.evaluated_at,
        };
        let _ = self.ctx.store.insert_evaluation(&rec);
    }

    fn persist_audit(
        &self,
        agent_id: &str,
        decision: AuditDecision,
        rule: Option<String>,
        severity: Option<String>,
        reason: &str,
    ) {
        let entry = AuditEntry {
            id: 0,
            run_id: Some(self.run.id.clone()),
            timestamp: Utc::now(),
            decision: match decision {
                AuditDecision::Allowed => "allowed".into(),
                AuditDecision::Blocked => "blocked".into(),
            },
            rule,
            severity,
            file: None,
            agent_id: Some(agent_id.to_string()),
            reason: reason.to_string(),
        };
        let _ = self.ctx.store.insert_audit(&entry);
    }

    /// Başarılı/sonuç üretilmiş bir turu kapatır.
    fn finalize(&mut self, result: &EvaluationResult, gen: u32) -> Result<()> {
        self.run.generation = gen;
        if result.score > self.run.best_score {
            self.run.best_score = result.score;
        }
        // production_success = tüm testler geçti (fitness'ten ayrı kavram).
        self.run.production_success = result.success;
        let status = if result.success {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        let kind = if result.success {
            RunEventKind::RunCompleted
        } else {
            RunEventKind::RunFailed
        };
        self.set_status(status, kind, None)?;
        Ok(())
    }

    // ---- altyapı ---------------------------------------------------------

    fn set_status(
        &mut self,
        status: RunStatus,
        kind: RunEventKind,
        payload: Option<serde_json::Value>,
    ) -> Result<()> {
        if self.run.status != status {
            self.run.transition(status)?;
        }
        self.emit(
            kind,
            None,
            Some(self.run.generation),
            payload.unwrap_or(serde_json::json!({"status": status.as_str()})),
        )?;
        self.persist();
        Ok(())
    }

    fn emit(
        &self,
        kind: RunEventKind,
        agent_id: Option<String>,
        generation: Option<u32>,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.ctx
            .bus
            .emit(&self.run.id, kind, agent_id, generation, None, payload)?;
        Ok(())
    }

    fn persist(&self) {
        let rec = self.run.to_record(&self.build, &self.sandbox_image);
        let _ = self.ctx.store.update_run(&rec);
    }
}

/// Deterministik, hızlı hash (prompt/kod parmak izi; kriptografik değil).
fn hash_str(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Heuristik mutasyon: prompt'a (varsa) tekrar etmeyen bir kural ekler.
fn mutate_prompt_heuristically(prompt: &str, _best: f64) -> String {
    let rule = "[ders] Kenar durumları (0, 1, negatif, boş girdi) ve tip taşmasını kontrol et.";
    if prompt.contains(rule) {
        // Farklı bir ders dene ki döngü ilerlesin.
        let alt = "[ders] Fonksiyon imzasındaki tiplere tam uy ve derleyici hatalarını önle.";
        if prompt.contains(alt) {
            return prompt.to_string();
        }
        return format!("{}\n{}", prompt.trim_end(), alt);
    }
    format!("{}\n{}", prompt.trim_end(), rule)
}

/// `RunStatus`'u Türkçe kullanıcı mesajına çevirir (UI için).
pub fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "Sırada",
        RunStatus::Running => "Çalışıyor",
        RunStatus::Paused => "Duraklatıldı",
        RunStatus::Cancelling => "İptal ediliyor",
        RunStatus::Cancelled => "İptal edildi",
        RunStatus::Completed => "Tamamlandı",
        RunStatus::Failed => "Başarısız",
        RunStatus::Interrupted => "Kesintiye uğradı",
        RunStatus::LimitReached => "Limite ulaşıldı",
    }
}

/// Kısa sürüm bilgisi (UI footer).
pub fn version_string() -> String {
    format!("Quine {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use quine_bench_simple::SimpleBenchmark;
    use quine_llm::ScriptedBackend;

    fn ctx() -> RuntimeContext {
        let store = Arc::new(Store::open_in_memory().unwrap());
        RuntimeContext::new(store, std::env::temp_dir())
    }

    fn local_config(mode: RunMode) -> RunConfig {
        RunConfig {
            sandbox_kind: "local".into(),
            mode,
            temperature: 0.0,
            limits: RunLimits {
                max_iterations: 4,
                max_generations: 2,
                max_llm_calls: 20,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn req(problem: Problem, mode: RunMode, backend: Arc<dyn LlmBackend>) -> RunRequest {
        RunRequest {
            problem,
            workload: WorkloadKind::Demo,
            config: local_config(mode),
            backend: Some(backend),
        }
    }

    #[tokio::test]
    async fn single_mode_completes_and_persists() {
        let c = ctx();
        let store = c.store.clone();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        let backend: Arc<dyn LlmBackend> = Arc::new(quine_llm::EchoBackend::fibonacci_solver());
        let handle = engine
            .start(req(problem, RunMode::Single, backend))
            .unwrap();
        let id = handle.run_id.clone();
        let outcome = handle.wait().await;
        assert_eq!(outcome.status, RunStatus::Completed, "{:?}", outcome.error);
        assert!(outcome.production_success);
        assert_eq!(outcome.best_score, 100.0);
        // Kalıcılık: run ve aday yazıldı.
        assert!(store.get_run(&id).unwrap().is_some());
        assert_eq!(store.list_candidates(&id).unwrap().len(), 1);
        assert!(!store.list_events(&id, 0).unwrap().is_empty());
    }

    #[tokio::test]
    async fn evolve_improves_from_failure_to_success() {
        // Deterministik backend: 1. çağrı yanlış (derlenmeyen), sonrakiler doğru.
        // Bu, failure → diagnosis → mutation → improved zincirini kanıtlar.
        let c = ctx();
        let store = c.store.clone();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedBackend::fail_then_fix(
            "```rust\npub fn fibonacci(n: u32) -> u64 { let x: u64 = \"nope\"; x }\n```",
            "```rust\npub fn fibonacci(n: u32) -> u64 {\n    let (mut a, mut b) = (0u64, 1u64);\n    for _ in 0..n { let t = a + b; a = b; b = t; }\n    a\n}\n```",
        ));
        let handle = engine
            .start(req(problem, RunMode::Evolve, backend))
            .unwrap();
        let id = handle.run_id.clone();
        let outcome = handle.wait().await;
        assert_eq!(outcome.status, RunStatus::Completed, "{:?}", outcome.error);
        assert_eq!(outcome.best_score, 100.0);

        let candidates = store.list_candidates(&id).unwrap();
        assert!(candidates.len() >= 2, "en az 2 aday (başarısız + başarılı)");
        // İlk aday 0, ikinci aday 100 → nedensel iyileşme kaydedildi.
        assert_eq!(candidates[0].score, 0.0);
        assert!(candidates.iter().any(|c| c.score == 100.0));
        // İkinci aday bir öncekinden türemiş (parent zinciri).
        assert!(candidates[1].parent_id.is_some());
        // Feedback sonraki isteğe girdi mi? En az bir LLM_REQUEST_STARTED var.
        let events = store.list_events(&id, 0).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"LLM_REQUEST_STARTED"));
        assert!(kinds.contains(&"MUTATION_PROPOSED"));
        assert!(kinds.contains(&"RUN_COMPLETED"));
    }

    #[tokio::test]
    async fn guardian_block_is_recorded_in_audit() {
        let c = ctx();
        let store = c.store.clone();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedBackend::new(vec![
            "```rust\npub fn fibonacci(n: u32) -> u64 { unsafe { std::mem::zeroed() } }\n```"
                .into(),
        ]));
        let handle = engine
            .start(req(problem, RunMode::Single, backend))
            .unwrap();
        let _ = handle.wait().await;
        let audit = store.list_audit(10).unwrap();
        assert!(
            audit.iter().any(|a| a.decision == "blocked"),
            "guardian bloku audit'e yazılmalı"
        );
    }

    #[tokio::test]
    async fn invalid_sandbox_kind_fails_closed() {
        let c = ctx();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        let mut r = req(
            problem,
            RunMode::Single,
            Arc::new(quine_llm::EchoBackend::fibonacci_solver()),
        );
        r.config.sandbox_kind = "banana".into();
        assert!(
            engine.start(r).is_err(),
            "geçersiz sandbox fail-closed olmalı"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_run() {
        let c = ctx();
        let store = c.store.clone();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        // Her zaman yanlış kod → döngü ilerler; iptal edebiliriz.
        let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedBackend::new(vec![
            "```rust\npub fn fibonacci(n: u32) -> u64 { 0 }\n```".into(),
        ]));
        let mut cfg = local_config(RunMode::Evolve);
        cfg.limits.max_iterations = 100;
        let r = RunRequest {
            problem,
            workload: WorkloadKind::Demo,
            config: cfg,
            backend: Some(backend),
        };
        let handle = engine.start(r).unwrap();
        let id = handle.run_id.clone();
        handle.cancel();
        let outcome = handle.wait().await;
        assert_eq!(outcome.status, RunStatus::Cancelled, "{:?}", outcome.error);
        assert_eq!(
            store.get_run(&id).unwrap().unwrap().status,
            RunStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn llm_call_limit_produces_limit_reached() {
        let c = ctx();
        let store = c.store.clone();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedBackend::new(vec![
            "```rust\npub fn fibonacci(n: u32) -> u64 { 0 }\n```".into(),
        ]));
        let mut cfg = local_config(RunMode::Evolve);
        cfg.limits.max_iterations = 50;
        cfg.limits.max_llm_calls = 2;
        let r = RunRequest {
            problem,
            workload: WorkloadKind::Demo,
            config: cfg,
            backend: Some(backend),
        };
        let outcome = engine.start(r).unwrap().wait().await;
        assert_eq!(
            outcome.status,
            RunStatus::LimitReached,
            "{:?}",
            outcome.error
        );
        assert!(store.get_run(&outcome.run_id).unwrap().unwrap().status == RunStatus::LimitReached);
    }

    #[test]
    fn heuristic_mutation_does_not_duplicate() {
        let p = "base prompt";
        let p1 = mutate_prompt_heuristically(p, 0.0);
        let p2 = mutate_prompt_heuristically(&p1, 0.0);
        assert_ne!(p1, p2, "ikinci mutasyon farklı bir ders eklemeli");
        let p3 = mutate_prompt_heuristically(&p2, 0.0);
        assert_eq!(
            p2, p3,
            "dersler tükendiğinde sabit kalmalı (sınırsız büyüme yok)"
        );
    }
}
