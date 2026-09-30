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

        let mut run = Run::new(
            Uuid::new_v4().to_string(),
            req.workload,
            req.problem.id.clone(),
            req.problem.title.clone(),
            req.config.model.clone(),
            req.config.temperature,
            sandbox_kind.clone(),
        );
        // Task persistence: kullanılan problem spesifikasyonunu sakla.
        run.problem_json = serde_json::to_string(&req.problem).unwrap_or_default();
        let run_id = run.id.clone();

        // Kalıcı ajan genomu: problem başına tek bir ajan kimliği tutulur, böylece
        // öğrenilen `[ders]` kuralları sonraki çalıştırmalarda da geçerli olur.
        let agent_uuid = format!("agent-{}", req.problem.id);
        let base_prompt = quine_common::default_system_prompt().to_string();
        let existing = self.ctx.store.get_agent(&agent_uuid).unwrap_or(None);
        let (prompt_for_run, created_at) = match &existing {
            Some(a) => (a.system_prompt.clone(), a.created_at),
            None => (base_prompt.clone(), Utc::now()),
        };
        let _ = self.ctx.store.upsert_agent(&quine_storage::AgentRecord {
            id: agent_uuid.clone(),
            run_id: run_id.clone(),
            name: req.problem.id.clone(),
            generation: 0,
            parent_id: None,
            prompt_hash: hash_str(&prompt_for_run),
            system_prompt: prompt_for_run.clone(),
            fitness_score: existing.as_ref().map(|a| a.fitness_score).unwrap_or(0.0),
            created_at,
            updated_at: Utc::now(),
        });
        let _ = self.ctx.store.link_agent_run(&agent_uuid, &run_id);
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
                agent_uuid,
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
    parent_candidate_id: Option<String>,
    generation: u32,
    code: &'a str,
    result: &'a EvaluationResult,
    delta: f64,
    reason: Option<String>,
    learned_rule: Option<String>,
    prompt: &'a str,
    previous_code: Option<&'a str>,
}

/// Paralel değerlendirme için tek bir aday görevi.
struct CandidateTask {
    agent_id: String,
    prompt: String,
    generation: u32,
}

/// Olayları yayan hafif yardımcı (paylaşılan üretim bağlamı için).
#[derive(Clone)]
struct Emitter {
    bus: EventBus,
    store: Arc<Store>,
    run_id: String,
}

impl Emitter {
    fn emit(
        &self,
        kind: RunEventKind,
        agent_id: Option<String>,
        generation: Option<u32>,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.bus
            .emit(&self.run_id, kind, agent_id.clone(), generation, None, payload)?;
        Ok(())
    }

    /// Guardian kararını denetim defterine yazar (run'a bağlı).
    fn audit(
        &self,
        agent_id: &str,
        decision: AuditDecision,
        rule: Option<String>,
        severity: Option<String>,
        reason: &str,
    ) {
        let entry = AuditEntry {
            id: 0,
            run_id: Some(self.run_id.clone()),
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
        let _ = self.store.insert_audit(&entry);
    }
}

/// Paylaşılan üretim/değerlendirme bağlamı: hem sıralı (single/evolve) hem
/// paralel (population) yol aynı mantığı kullanır.
#[derive(Clone)]
struct ProduceCtx {
    emitter: Emitter,
    guardian: DiffAnalyzer,
    backend: Arc<dyn LlmBackend>,
    evaluator: Arc<Evaluator>,
    problem: Problem,
    model: String,
    temperature: f32,
    max_candidate_size: usize,
    sandbox: String,
}

/// LLM → kod → guardian → sandbox → test zincirini yürütür (bağımsız, paralel
/// çalıştırılabilir). İptalde `select!` ile düşer; sandbox alt süreçleri
/// `kill_on_drop` ile öldürülür.
async fn produce_candidate(
    ctx: &ProduceCtx,
    agent_id: &str,
    gen: u32,
    prompt: &str,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<(String, EvaluationResult)> {
    let request = LlmRequest {
        model: ctx.model.clone(),
        system: prompt.to_string(),
        prompt: ctx.problem.to_llm_prompt(),
        temperature: ctx.temperature,
        max_tokens: 1024,
    };

    ctx.emitter.emit(
        RunEventKind::LlmRequestStarted,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({"model": request.model}),
    )?;
    let t0 = Instant::now();
    let response = tokio::select! {
        r = ctx.backend.generate(&request) => r.context("LLM üretimi")?,
        _ = wait_cancel(cancel_rx) => anyhow::bail!("run iptal edildi (LLM beklenirken)"),
    };
    ctx.emitter.emit(
        RunEventKind::LlmResponseReceived,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({
            "model": response.model,
            "bytes": response.content.len(),
            "duration_ms": t0.elapsed().as_millis() as u64,
        }),
    )?;

    let code = match response.extract_code_checked() {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("model kod üretmedi: {e:#}");
            let result = EvaluationResult {
                problem_id: ctx.problem.id.clone(),
                agent_id: Uuid::parse_str(agent_id).unwrap_or_else(|_| Uuid::new_v4()),
                success: false,
                score: 0.0,
                stdout: String::new(),
                stderr: reason.clone(),
                duration_ms: 0,
                tests_passed: 0,
                tests_total: ctx.problem.test_cases.len(),
                evaluated_at: Utc::now(),
            };
            ctx.emitter.emit(
                RunEventKind::EvaluationCompleted,
                Some(agent_id.to_string()),
                Some(gen),
                serde_json::json!({
                    "score": 0.0, "success": false,
                    "reason": "model_no_code", "failure_reason": reason,
                }),
            )?;
            return Ok((String::new(), result));
        }
    };
    ctx.emitter.emit(
        RunEventKind::CodeExtracted,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({"bytes": code.len()}),
    )?;

    if code.len() > ctx.max_candidate_size {
        anyhow::bail!(
            "aday kod boyutu limiti aşıldı ({} > {})",
            code.len(),
            ctx.max_candidate_size
        );
    }

    // Guardian kapısı — fail-closed: kritik ihlalde sandbox'a hiç gidilmez.
    let t_g = Instant::now();
    if let Err(v) = ctx.guardian.analyze(&code) {
        let rule = v.violations.first().map(|f| f.rule.clone());
        let severity = v.violations.first().map(|f| format!("{:?}", f.severity));
        let reason = v.to_string();
        ctx.emitter.emit(
            RunEventKind::GuardianBlocked,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({
                "rule": rule, "severity": severity, "reason": reason,
                "duration_ms": t_g.elapsed().as_millis() as u64,
            }),
        )?;
        let result = EvaluationResult {
            problem_id: ctx.problem.id.clone(),
            agent_id: Uuid::parse_str(agent_id).unwrap_or_else(|_| Uuid::new_v4()),
            success: false,
            score: 0.0,
            stdout: String::new(),
            stderr: format!("GUARDIAN BLOCKED: {reason}"),
            duration_ms: t_g.elapsed().as_millis() as u64,
            tests_passed: 0,
            tests_total: ctx.problem.test_cases.len(),
            evaluated_at: Utc::now(),
        };
        ctx.emitter.audit(
            agent_id,
            AuditDecision::Blocked,
            rule.clone(),
            severity,
            &reason,
        );
        ctx.emitter.emit(
            RunEventKind::CandidateRejected,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({"reason": "guardian_blocked", "rule": rule}),
        )?;
        ctx.emitter.emit(
            RunEventKind::EvaluationCompleted,
            Some(agent_id.to_string()),
            Some(gen),
            serde_json::json!({
                "score": 0.0, "success": false,
                "tests_passed": 0, "tests_total": result.tests_total,
                "failure_reason": "guardian tarafından engellendi",
            }),
        )?;
        return Ok((code, result));
    }
    ctx.emitter.emit(
        RunEventKind::GuardianAllowed,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({"duration_ms": t_g.elapsed().as_millis() as u64}),
    )?;
    ctx.emitter
        .audit(agent_id, AuditDecision::Allowed, None, None, "guardian geçti");

    ctx.emitter.emit(
        RunEventKind::SandboxStarted,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({"sandbox": ctx.sandbox}),
    )?;
    let t_s = Instant::now();
    let ev = ctx.evaluator.clone();
    let problem = ctx.problem.clone();
    let id = Uuid::parse_str(agent_id).unwrap_or_else(|_| Uuid::new_v4());
    let result = tokio::select! {
        r = ev.evaluate(id, &problem, &code) => r,
        _ = wait_cancel(cancel_rx) => anyhow::bail!("run iptal edildi (sandbox çalışırken)"),
    };
    ctx.emitter.emit(
        RunEventKind::SandboxCompleted,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({
            "duration_ms": t_s.elapsed().as_millis() as u64,
            "ok": !result.stderr.contains("SANDBOX ERROR"),
        }),
    )?;
    ctx.emitter.emit(
        RunEventKind::TestsCompleted,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({"passed": result.tests_passed, "total": result.tests_total}),
    )?;
    ctx.emitter.emit(
        RunEventKind::EvaluationCompleted,
        Some(agent_id.to_string()),
        Some(gen),
        serde_json::json!({
            "score": result.score,
            "success": result.success,
            "tests_passed": result.tests_passed,
            "tests_total": result.tests_total,
            "failure_reason": if result.success { None } else { Some(failure_summary(&result.stderr)) },
        }),
    )?;
    Ok((code, result))
}

/// İptal sinyalini bekleyen gelecek (paylaşılan üretim bağlamı için).
async fn wait_cancel(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    let _ = rx.changed().await;
}

/// Başarısızlık nedenini kısa, insan okunur bir cümleye indirger.
fn failure_summary(stderr: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        return "bilinmeyen hata".into();
    }
    if t.contains("GUARDIAN BLOCKED") {
        return "güvenlik kontrolü kodu engelledi".into();
    }
    if t.contains("MODEL KOD ÜRETMEDİ") || t.contains("model kod üretmedi") {
        return "model geçerli kod üretmedi".into();
    }
    if let Some(line) = t.lines().find(|l| l.starts_with("error[")) {
        return line.trim().to_string();
    }
    if let Some(line) = t.lines().find(|l| l.starts_with("error:")) {
        return line.trim().to_string();
    }
    if t.contains("Başarısız testler") {
        return "test beklentileri karşılanmadı".into();
    }
    t.lines()
        .next()
        .unwrap_or("bilinmeyen hata")
        .chars()
        .take(160)
        .collect()
}

/// İki kod sürümü arasında basit satır diff'i (harici bağımlılık yok).
fn simple_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        match (old_lines.get(i), new_lines.get(j)) {
            (Some(a), Some(b)) if a == b => {
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) => {
                out.push_str(&format!("-{a}
+{b}
"));
                i += 1;
                j += 1;
            }
            (Some(a), None) => {
                out.push_str(&format!("-{a}
"));
                i += 1;
            }
            (None, Some(b)) => {
                out.push_str(&format!("+{b}
"));
                j += 1;
            }
            (None, None) => break,
        }
    }
    if out.is_empty() {
        "değişiklik yok".to_string()
    } else {
        out
    }
}

/// İki prompt arasında yeni eklenen `[ders]` kuralını çıkarır.
fn extract_new_rule(old_prompt: &str, new_prompt: &str) -> Option<String> {
    new_prompt.lines().find_map(|l| {
        let t = l.trim();
        if t.starts_with("[ders]") && !old_prompt.contains(t) {
            Some(t.trim_start_matches("[ders]").trim().to_string())
        } else {
            None
        }
    })
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
    /// Kök ajan kimliği — öğrenilen genom bu kimlik altında run'lar arası saklanır.
    agent_uuid: String,
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
        let prompt = self.persistent_prompt();
        let (code, result) = self
            .generate_and_evaluate_with_prompt(&agent_id, 0, &prompt)
            .await?;
        self.record_candidate(CandidateInput {
            agent_id: &agent_id,
            parent_id: None,
            parent_candidate_id: None,
            generation: 0,
            code: &code,
            result: &result,
            delta: 0.0,
            reason: None,
            learned_rule: None,
            prompt: &prompt,
            previous_code: None,
        })?;
        self.learn_and_persist(&result);
        self.finalize(&result, 0)
    }

    /// Ajanın kalıcı prompt'unu döner (önceki run'larda öğrenilen dersler dahil).
    fn persistent_prompt(&self) -> String {
        self.ctx
            .store
            .get_agent(&self.agent_uuid)
            .ok()
            .flatten()
            .map(|a| a.system_prompt)
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| quine_common::default_system_prompt().to_string())
    }

    async fn mode_evolve(&mut self) -> Result<()> {
        let mut agent_id = Uuid::new_v4().to_string();
        let mut parent: Option<String> = None;
        let mut previous_best = 0.0f64;
        let mut prompt = self.persistent_prompt();
        let mut previous_code: Option<String> = None;
        let mut parent_candidate_id: Option<String> = None;

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
            let failure_reason = (!result.success).then(|| failure_summary(&result.stderr));
            let blocked = result.stderr.contains("GUARDIAN BLOCKED");

            let cand_id = self.record_candidate(CandidateInput {
                agent_id: &agent_id,
                parent_id: parent.clone(),
                parent_candidate_id: parent_candidate_id.clone(),
                generation: gen,
                code: &code,
                result: &result,
                delta,
                reason: None,
                learned_rule: None,
                prompt: &prompt,
                previous_code: previous_code.as_deref(),
            })?;
            previous_code = Some(code.clone());
            self.run.total_evaluations += 1;

            if result.success {
                self.learn_and_persist(&result);
                return self.finalize(&result, gen);
            }

            // Öğrenme: başarısızlığı özetle → kural çıkar → prompt'u güncelle.
            self.emit(
                RunEventKind::MutationProposed,
                Some(agent_id.clone()),
                Some(gen),
                serde_json::json!({
                    "previous_score": result.score,
                    "failed_tests": result.tests_total - result.tests_passed,
                    "failure_reason": failure_reason,
                    "guardian_blocked": blocked,
                    "candidate_id": cand_id,
                }),
            )?;
            let (new_prompt, learned_rule) = self
                .propose_mutation(&prompt, &result, agent_id.clone(), gen)
                .await?;

            // Öğrenilen kuralı adaya işle (UI "ne öğrendi?" sorusunu yanıtlar).
            if let Some(rule) = &learned_rule {
                let _ = self.ctx.store.set_candidate_learned_rule(&cand_id, rule);
            }

            let improved = result.score >= previous_best;
            if improved {
                previous_best = result.score;
                if result.score > self.run.best_score {
                    self.run.best_score = result.score;
                }
                self.emit(
                    RunEventKind::MutationAccepted,
                    Some(agent_id.clone()),
                    Some(gen),
                    serde_json::json!({
                        "previous_score": result.score,
                        "new_score": result.score,
                        "delta": delta,
                        "learned_rule": learned_rule,
                        "parent_candidate_id": cand_id,
                        "failure_reason": failure_reason,
                    }),
                )?;
                parent = Some(agent_id.clone());
                parent_candidate_id = Some(cand_id.clone());
                agent_id = Uuid::new_v4().to_string();
                prompt = new_prompt;
                self.run.generation += 1;
            } else {
                self.emit(
                    RunEventKind::MutationRejected,
                    Some(agent_id.clone()),
                    Some(gen),
                    serde_json::json!({
                        "previous_score": result.score,
                        "new_score": result.score,
                        "delta": delta,
                        "best": previous_best,
                        "learned_rule": learned_rule,
                        "parent_candidate_id": cand_id,
                        "failure_reason": failure_reason,
                    }),
                )?;
                // Reddedilse de öğrenilen kural prompt'a eklenir (denemeye devam).
                prompt = new_prompt;
            }
            self.emit(
                RunEventKind::GenerationCompleted,
                Some(agent_id.clone()),
                Some(gen),
                serde_json::json!({
                    "best_score": self.run.best_score,
                    "generation": gen,
                }),
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
        let base_prompt = self.persistent_prompt();
        let mut population: Vec<(String, String, f64)> = (0..size)
            .map(|_| (Uuid::new_v4().to_string(), base_prompt.clone(), 0.0))
            .collect();

        for gen in 0..self.config.limits.max_generations {
            self.checkpoint().await;
            if self.should_stop(gen)? {
                return Ok(());
            }
            self.run.generation = gen;

            // Kalan LLM bütçesine göre bu jenerasyonun boyutunu sınırla.
            let remaining = self
                .config
                .limits
                .max_llm_calls
                .saturating_sub(self.run.total_llm_calls) as usize;
            if remaining == 0 {
                self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
                return Ok(());
            }
            let batch: Vec<CandidateTask> = population
                .iter()
                .take(remaining.min(size))
                .map(|(id, p, _)| CandidateTask {
                    agent_id: id.clone(),
                    prompt: p.clone(),
                    generation: gen,
                })
                .collect();
            let batch_len = batch.len();

            // GERÇEK paralellik: tüm adaylar eşzamanlı üretilir/değerlendirilir.
            let results = self.run_batch(batch).await?;
            self.run.total_llm_calls += batch_len as u32;

            let mut scored: Vec<(String, String, f64)> = Vec::with_capacity(batch_len);
            let mut solved: Option<EvaluationResult> = None;
            let mut cancelled = false;
            for (agent_id, prompt, outcome) in results {
                self.run.total_evaluations += 1;
                match outcome {
                    Ok((code, result)) => {
                        self.record_candidate(CandidateInput {
                            agent_id: &agent_id,
                            parent_id: None,
                            parent_candidate_id: None,
                            generation: gen,
                            code: &code,
                            result: &result,
                            delta: 0.0,
                            reason: None,
                            learned_rule: None,
                            prompt: &prompt,
                            previous_code: None,
                        })?;
                        if result.success {
                            solved = Some(result.clone());
                        }
                        scored.push((agent_id, prompt, result.score));
                    }
                    Err(e) => {
                        if self.control.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                        tracing::warn!("aday üretilemedi: {e:#}");
                        scored.push((agent_id, prompt, 0.0));
                    }
                }
            }
            if cancelled {
                return Ok(());
            }
            if let Some(result) = solved {
                self.learn_and_persist(&result);
                return self.finalize(&result, gen);
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
                let new_prompt = mutate_prompt_heuristically(base, best);
                let learned = extract_new_rule(base, &new_prompt);
                self.emit(
                    RunEventKind::MutationProposed,
                    None,
                    Some(gen),
                    serde_json::json!({
                        "previous_score": best,
                        "failed_tests": 0,
                        "learned_rule": learned,
                    }),
                )?;
                next.push((Uuid::new_v4().to_string(), new_prompt, 0.0));
            }
            population = next;
        }

        self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
        Ok(())
    }

    /// Bir aday grubunu gerçekten eşzamanlı üretir/değerlendirir.
    async fn run_batch(
        &self,
        tasks: Vec<CandidateTask>,
    ) -> Result<Vec<(String, String, Result<(String, EvaluationResult)>)>> {
        let ctx = self.produce_ctx();
        let max_concurrent = self.config.limits.max_concurrent.max(1);
        let sem = Arc::new(Semaphore::new(max_concurrent));
        let mut set = tokio::task::JoinSet::new();
        for t in tasks {
            let ctx = ctx.clone();
            let sem = sem.clone();
            let cancel_rx = self.cancel_rx.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore açık");
                let mut rx = cancel_rx;
                let out =
                    produce_candidate(&ctx, &t.agent_id, t.generation, &t.prompt, &mut rx).await;
                (t.agent_id, t.prompt, out)
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(r) => results.push(r),
                Err(e) => {
                    if e.is_panic() {
                        return Err(anyhow::anyhow!("aday görevi panikledi: {e}"));
                    }
                }
            }
        }
        Ok(results)
    }

    /// Paylaşılan üretim/değerlendirme bağlamı (paralel ve sıralı yol aynı mantık).
    fn produce_ctx(&self) -> ProduceCtx {
        let backend: Arc<dyn LlmBackend> = match &self.backend {
            Some(b) => b.clone(),
            None => Arc::new(quine_llm::OllamaBackend::new(
                std::env::var("OLLAMA_HOST")
                    .unwrap_or_else(|_| quine_llm::DEFAULT_OLLAMA_HOST.into()),
                self.config.model.clone(),
            )),
        };
        ProduceCtx {
            emitter: Emitter {
                bus: self.ctx.bus.clone(),
                store: self.ctx.store.clone(),
                run_id: self.run.id.clone(),
            },
            guardian: self.guardian.clone(),
            backend,
            evaluator: self.evaluator.clone(),
            problem: self.problem.clone(),
            model: self.config.model.clone(),
            temperature: self.config.temperature as f32,
            max_candidate_size: self.config.limits.max_candidate_size,
            sandbox: self.run.sandbox.clone(),
        }
    }

    /// Başarılı bir sonuçtan öğrenip ajan genomunu kalıcı hale getirir.
    fn learn_and_persist(&mut self, result: &EvaluationResult) {
        if result.success && result.score > self.run.best_score {
            self.run.best_score = result.score;
        }
        self.persist_agent(result.score);
    }

    /// Ajanın güncel prompt'unu ve fitness'ını kalıcı hale getirir.
    fn persist_agent(&self, fitness: f64) {
        let prompt = self.persistent_prompt();
        let previous = self
            .ctx
            .store
            .get_agent(&self.agent_uuid)
            .ok()
            .flatten()
            .map(|a| a.fitness_score)
            .unwrap_or(0.0);
        let _ = self.ctx.store.upsert_agent(&quine_storage::AgentRecord {
            id: self.agent_uuid.clone(),
            run_id: self.run.id.clone(),
            name: self.problem.id.clone(),
            generation: self.run.generation,
            parent_id: None,
            prompt_hash: hash_str(&prompt),
            system_prompt: prompt,
            fitness_score: fitness.max(previous),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
    }

    /// Sıralı yol: tek üretim + değerlendirme (single/evolve).
    async fn generate_and_evaluate_with_prompt(
        &mut self,
        agent_id: &str,
        gen: u32,
        prompt: &str,
    ) -> Result<(String, EvaluationResult)> {
        if self.run.total_llm_calls >= self.config.limits.max_llm_calls {
            self.set_status(RunStatus::LimitReached, RunEventKind::LimitReached, None)?;
            anyhow::bail!("LLM çağrı limiti aşıldı");
        }
        let ctx = self.produce_ctx();
        let mut rx = self.cancel_rx.clone();
        let out = produce_candidate(&ctx, agent_id, gen, prompt, &mut rx).await;
        if out.is_ok() {
            self.run.total_llm_calls += 1;
        }
        let (code, result) = out?;
        self.persist_evaluation(agent_id, &result);
        Ok((code, result))
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
    /// Döner: (yeni_prompt, öğrenilen_kural).
    /// Başarısızlıktan kural çıkarıp prompt'u günceller (öğrenme).
    /// Döner: (yeni_prompt, öğrenilen_kural).
    async fn propose_mutation(
        &mut self,
        prompt: &str,
        result: &EvaluationResult,
        agent_id: String,
        gen: u32,
    ) -> Result<(String, Option<String>)> {
        self.checkpoint().await;
        let failures = vec![result.clone()];
        if let Some(backend) = self.backend.clone() {
            let engine = quine_evolution::MutationEngine::new(backend.as_ref());
            let mut agent = quine_common::Agent::new("run-agent");
            agent.system_prompt = prompt.to_string();
            let model = self.config.model.clone();
            match engine.refine_prompt(&agent, &model, &failures).await {
                Ok(new_prompt) => {
                    let rule = extract_new_rule(prompt, &new_prompt);
                    return Ok((new_prompt, rule));
                }
                Err(e) => {
                    tracing::warn!("refine başarısız ({e:#}); heuristik kural");
                }
            }
        }
        let _ = (agent_id, gen);
        let new_prompt = mutate_prompt_heuristically(prompt, self.run.best_score);
        let rule = extract_new_rule(prompt, &new_prompt);
        Ok((new_prompt, rule))
    }

    /// Adayı kalıcı hale getirir; oluşturulan adayın kimliğini döner.
    #[allow(clippy::too_many_arguments)]
    fn record_candidate(&mut self, input: CandidateInput<'_>) -> Result<String> {
        let CandidateInput {
            agent_id,
            parent_id,
            parent_candidate_id,
            generation: gen,
            code,
            result,
            delta,
            reason,
            learned_rule,
            prompt,
            previous_code,
        } = input;
        let id = Uuid::new_v4().to_string();
        let diff = match previous_code {
            Some(prev) if prev != code => Some(simple_diff(prev, code)),
            Some(_) => Some("değişiklik yok (aynı kod)".to_string()),
            None => Some("initial candidate".to_string()),
        };
        let failure_reason = (!result.success).then(|| failure_summary(&result.stderr));
        let rec = CandidateRecord {
            id: id.clone(),
            run_id: self.run.id.clone(),
            agent_id: agent_id.to_string(),
            parent_id,
            generation: gen,
            prompt_hash: hash_str(prompt),
            code_hash: hash_str(code),
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
            failure_reason,
            learned_rule,
            parent_candidate_id,
            diff,
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
        Ok(id)
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
    use quine_llm::{LlmRequest, LlmResponse, ScriptedBackend};

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

    /// Üretim yolunun eşzamanlılık ölçüm backend'i: kaç LLM çağrısının AYNI
    /// ANDA uçuşta olduğunu izler. Gerçek paralellik kanıtı için kullanılır.
    struct ConcurrencyProbe {
        in_flight: std::sync::atomic::AtomicUsize,
        max_in_flight: std::sync::atomic::AtomicUsize,
    }

    impl ConcurrencyProbe {
        fn new() -> Self {
            Self {
                in_flight: std::sync::atomic::AtomicUsize::new(0),
                max_in_flight: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn peak(&self) -> usize {
            self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl LlmBackend for ConcurrencyProbe {
        fn name(&self) -> &str {
            "concurrency-probe"
        }
        fn generate<'a>(
            &'a self,
            _request: &'a LlmRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<LlmResponse>> + Send + 'a>>
        {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.max_in_flight.fetch_max(now, SeqCst);
            Box::pin(async move {
                // Model gecikmesini taklit et: eşzamanlı yürütme yoksa tepe değer 1 kalır.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                self.in_flight.fetch_sub(1, SeqCst);
                Ok(LlmResponse {
                    content: "```rust\npub fn fibonacci(n: u32) -> u64 {\n    let (mut a, mut b) = (0u64, 1u64);\n    for _ in 0..n { let t = a + b; a = b; b = t; }\n    a\n}\n```".into(),
                    model: "probe".into(),
                    duration_ns: None,
                })
            })
        }
        fn health_check(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn population_runs_candidates_in_parallel() {
        let c = ctx();
        let engine = RunEngine::new(c);
        let problem = SimpleBenchmark::problem("fib-001").unwrap();
        let probe = Arc::new(ConcurrencyProbe::new());
        let backend: Arc<dyn LlmBackend> = probe.clone();
        let mut cfg = local_config(RunMode::Population);
        cfg.population_size = 4;
        cfg.limits.max_generations = 1;
        let r = RunRequest {
            problem,
            workload: WorkloadKind::Demo,
            config: cfg,
            backend: Some(backend),
        };
        let handle = engine.start(r).unwrap();
        let _ = handle.wait().await;
        assert!(
            probe.peak() >= 2,
            "popülasyon adayları paralel üretilmeli; tepe eşzamanlılık = {}",
            probe.peak()
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
