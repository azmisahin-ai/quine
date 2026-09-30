//! # quine-storage
//!
//! Quine'ın **production kalıcılık katmanı**. Araştırma/demo için JSON dosyaları
//! kalabilir; ancak runtime state'in source of truth'u budur: SQLite (WAL,
//! foreign keys, migration, crash recovery).
//!
//! Tasarım ilkeleri:
//! * Tek `Store` = tek `Connection` (Mutex ile korunur); WAL sayesinde okuma
//!   yazma ile bloke olmaz.
//! * Şema sürümü [`app_metadata`] içinde tutulur ve açılışta **ileri yönlü**
//!   migration uygulanır ([`SCHEMA_VERSION`]).
//! * İdempotent yazımlar (`INSERT OR REPLACE`) ile crash sonrası yeniden
//!   yazma güvenlidir.
//!
//! Kritik güvenlik notu: bu katman **fail-closed**'dur — DB açılamazsa hata
//! döner, sessizce in-memory moda düşmez.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Geçerli şema sürümü. Şema değişince artırılır; migration adımları eklenir.
pub const SCHEMA_VERSION: i64 = 1;

/// Bir run'ın yaşam döngüsü durumu (bkz. `quine-runtime` state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Paused,
    Cancelling,
    Cancelled,
    Completed,
    Failed,
    Interrupted,
    LimitReached,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Paused => "paused",
            RunStatus::Cancelling => "cancelling",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Interrupted => "interrupted",
            RunStatus::LimitReached => "limit_reached",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => RunStatus::Queued,
            "running" => RunStatus::Running,
            "paused" => RunStatus::Paused,
            "cancelling" => RunStatus::Cancelling,
            "cancelled" => RunStatus::Cancelled,
            "completed" => RunStatus::Completed,
            "failed" => RunStatus::Failed,
            "interrupted" => RunStatus::Interrupted,
            "limit_reached" => RunStatus::LimitReached,
            _ => return None,
        })
    }

    /// Çalışmanın hâlâ devam ettiği (aktif) durumlar.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            RunStatus::Queued | RunStatus::Running | RunStatus::Paused | RunStatus::Cancelling
        )
    }

    /// Terminal (artık değişmeyecek) durumda mı?
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunStatus::Completed
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::Interrupted
                | RunStatus::LimitReached
        )
    }
}

/// Çalışma tipi: güvenlik/retention davranışı buna göre farklılaşır.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    /// Deterministik demo/simülasyon (EchoBackend).
    Demo,
    /// Ölçüm amaçlı benchmark.
    Benchmark,
    /// Gerçek kullanıcı görevi (production).
    Production,
}

impl WorkloadKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkloadKind::Demo => "demo",
            WorkloadKind::Benchmark => "benchmark",
            WorkloadKind::Production => "production",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "demo" => WorkloadKind::Demo,
            "benchmark" => WorkloadKind::Benchmark,
            "production" => WorkloadKind::Production,
            _ => return None,
        })
    }
}

/// Bir run kaydının kalıcı hali.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub workload: WorkloadKind,
    pub problem_id: String,
    pub problem_title: String,
    pub model: String,
    pub temperature: f64,
    pub sandbox: String,
    pub generation: u32,
    pub iteration: u32,
    pub best_score: f64,
    pub best_agent_id: Option<String>,
    pub total_llm_calls: u32,
    pub total_evaluations: u32,
    pub production_success: bool,
    pub error: Option<String>,
    /// Determinizm/reproducibility için ortam parmak izi.
    pub quine_version: String,
    pub git_revision: String,
    pub sandbox_image: String,
}

/// Bir run'a ait tek bir yaşam döngüsü olayı (UI canlı akışı ve audit için).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub run_id: String,
    pub sequence: i64,
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub agent_id: Option<String>,
    pub generation: Option<u32>,
    pub duration_ms: Option<u64>,
    pub payload: serde_json::Value,
}

/// Üretilen/denenmiş aday kod ve sonucu (nedensel mutasyon takibi için).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub id: String,
    pub run_id: String,
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub generation: u32,
    pub prompt_hash: String,
    pub code: String,
    pub score: f64,
    pub tests_passed: usize,
    pub tests_total: usize,
    pub duration_ms: u64,
    pub model: String,
    pub status: String,
    pub accepted: bool,
    /// Bir önceki en iyi skora göre fark (nedensellik: "+12.5").
    pub delta: f64,
    pub mutation_reason: Option<String>,
    pub diff: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Guardian denetim kaydı.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub run_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub decision: String,
    pub rule: Option<String>,
    pub severity: Option<String>,
    pub file: Option<String>,
    pub agent_id: Option<String>,
    pub reason: String,
}

/// Değerlendirme sonucunun kalıcı kaydı.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRecord {
    pub id: i64,
    pub run_id: String,
    pub agent_id: String,
    pub problem_id: String,
    pub success: bool,
    pub score: f64,
    pub tests_passed: usize,
    pub tests_total: usize,
    pub duration_ms: u64,
    pub stderr: String,
    pub created_at: DateTime<Utc>,
}

/// Kayıtlı görev tanımı (problem + metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub kind: String,
    pub title: String,
    pub problem_json: String,
    pub source: String,
}

/// Retansiyon politikası (sınırsız büyümeyi engeller).
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// En fazla kaç run saklanır (eskiler budanır).
    pub max_runs: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self { max_runs: 500 }
    }
}

/// SQLite tabanlı kalıcılık deposu.
pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Store {
    /// Yeni bir depo açar (dosyayı ve şemayı gerekirse oluşturur).
    ///
    /// Fail-closed: dosya açılamaz veya migration başarısız olursa hata döner.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("storage dizini oluşturulamadı: {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("SQLite açılamadı: {}", path.display()))?;
        Self::configure(&conn)?;
        let store = Self {
            conn: Mutex::new(conn),
            path,
        };
        store.migrate()?;
        Ok(store)
    }

    /// Yalnızca testler için: bellekte geçici depo.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("in-memory SQLite açılamadı")?;
        Self::configure(&conn)?;
        let store = Self {
            conn: Mutex::new(conn),
            path: PathBuf::from(":memory:"),
        };
        store.migrate()?;
        Ok(store)
    }

    fn configure(conn: &Connection) -> Result<()> {
        // WAL: eşzamanlı okur/yazar; foreign_keys: bütünlük; busy_timeout: kilit bekle.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;",
        )
        .context("SQLite pragma ayarları uygulanamadı")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_metadata (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;

        let current: i64 = conn
            .query_row(
                "SELECT value FROM app_metadata WHERE key='schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if current < 1 {
            conn.execute_batch(SCHEMA_V1)
                .context("şema v1 uygulanamadı")?;
        }
        // Gelecekteki migration adımları burada sıralanır (current < 2 { ... }).

        conn.execute(
            "INSERT INTO app_metadata(key,value) VALUES('schema_version',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Açılışta yarıda kalmış run'ları `interrupted` işaretler (crash recovery).
    ///
    /// Sunucu yeniden başladığında aktif görünen ama gerçekte ölü olan run'lar
    /// sonsuza dek "running" kalmamalıdır.
    pub fn mark_orphans_interrupted(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE runs SET status='interrupted', finished_at=?1
             WHERE status IN ('queued','running','paused','cancelling')",
            params![Utc::now()],
        )?;
        Ok(n)
    }

    // ---- runs ------------------------------------------------------------

    pub fn insert_run(&self, r: &RunRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runs (
                id, created_at, started_at, finished_at, status, workload,
                problem_id, problem_title, model, temperature, sandbox,
                generation, iteration, best_score, best_agent_id,
                total_llm_calls, total_evaluations, production_success, error,
                quine_version, git_revision, sandbox_image
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                r.id, r.created_at, r.started_at, r.finished_at,
                r.status.as_str(), r.workload.as_str(),
                r.problem_id, r.problem_title, r.model, r.temperature, r.sandbox,
                r.generation, r.iteration, r.best_score, r.best_agent_id,
                r.total_llm_calls, r.total_evaluations, r.production_success as i64, r.error,
                r.quine_version, r.git_revision, r.sandbox_image,
            ],
        )?;
        Ok(())
    }

    /// Run'ın değişken alanlarını günceller (durum, sayaçlar, en iyi skor).
    pub fn update_run(&self, r: &RunRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE runs SET
                started_at=?2, finished_at=?3, status=?4,
                generation=?5, iteration=?6, best_score=?7, best_agent_id=?8,
                total_llm_calls=?9, total_evaluations=?10, production_success=?11, error=?12
             WHERE id=?1",
            params![
                r.id,
                r.started_at,
                r.finished_at,
                r.status.as_str(),
                r.generation,
                r.iteration,
                r.best_score,
                r.best_agent_id,
                r.total_llm_calls,
                r.total_evaluations,
                r.production_success as i64,
                r.error,
            ],
        )?;
        Ok(())
    }

    pub fn get_run(&self, id: &str) -> Result<Option<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(RUN_SELECT_WHERE)?;
        let row = stmt.query_row(params![id], row_to_run).optional()?;
        Ok(row)
    }

    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "{RUN_SELECT_BASE} ORDER BY created_at DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_run)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- events ----------------------------------------------------------

    /// Bir olayı run'a bağlı sıra numarasıyla ekler (atomik).
    pub fn append_event(&self, e: &EventRecord) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM run_events WHERE run_id=?1",
            params![e.run_id],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO run_events(run_id, sequence, timestamp, kind, agent_id, generation, duration_ms, payload)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                e.run_id, next, e.timestamp, e.kind, e.agent_id, e.generation,
                e.duration_ms.map(|d| d as i64),
                serde_json::to_string(&e.payload)?,
            ],
        )?;
        Ok(next)
    }

    pub fn list_events(&self, run_id: &str, after_sequence: i64) -> Result<Vec<EventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, sequence, timestamp, kind, agent_id, generation, duration_ms, payload
             FROM run_events WHERE run_id=?1 AND sequence>?2 ORDER BY sequence ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id, after_sequence], |r| {
                let payload: String = r.get(7)?;
                Ok(EventRecord {
                    run_id: r.get(0)?,
                    sequence: r.get(1)?,
                    timestamp: r.get(2)?,
                    kind: r.get(3)?,
                    agent_id: r.get(4)?,
                    generation: r.get::<_, Option<i64>>(5)?.map(|g| g as u32),
                    duration_ms: r.get::<_, Option<i64>>(6)?.map(|d| d as u64),
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- candidates ------------------------------------------------------

    pub fn insert_candidate(&self, c: &CandidateRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO candidates(
                id, run_id, agent_id, parent_id, generation, prompt_hash, code, score,
                tests_passed, tests_total, duration_ms, model, status, accepted, delta,
                mutation_reason, diff, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                c.id,
                c.run_id,
                c.agent_id,
                c.parent_id,
                c.generation,
                c.prompt_hash,
                c.code,
                c.score,
                c.tests_passed as i64,
                c.tests_total as i64,
                c.duration_ms as i64,
                c.model,
                c.status,
                c.accepted as i64,
                c.delta,
                c.mutation_reason,
                c.diff,
                c.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_candidates(&self, run_id: &str) -> Result<Vec<CandidateRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, agent_id, parent_id, generation, prompt_hash, code, score,
                    tests_passed, tests_total, duration_ms, model, status, accepted, delta,
                    mutation_reason, diff, created_at
             FROM candidates WHERE run_id=?1 ORDER BY generation ASC, created_at ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(CandidateRecord {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    parent_id: r.get(3)?,
                    generation: r.get::<_, i64>(4)? as u32,
                    prompt_hash: r.get(5)?,
                    code: r.get(6)?,
                    score: r.get(7)?,
                    tests_passed: r.get::<_, i64>(8)? as usize,
                    tests_total: r.get::<_, i64>(9)? as usize,
                    duration_ms: r.get::<_, i64>(10)? as u64,
                    model: r.get(11)?,
                    status: r.get(12)?,
                    accepted: r.get::<_, i64>(13)? != 0,
                    delta: r.get(14)?,
                    mutation_reason: r.get(15)?,
                    diff: r.get(16)?,
                    created_at: r.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- evaluations -----------------------------------------------------

    pub fn insert_evaluation(&self, e: &EvaluationRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO evaluations(run_id, agent_id, problem_id, success, score,
                tests_passed, tests_total, duration_ms, stderr, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                e.run_id,
                e.agent_id,
                e.problem_id,
                e.success as i64,
                e.score,
                e.tests_passed as i64,
                e.tests_total as i64,
                e.duration_ms as i64,
                e.stderr,
                e.created_at,
            ],
        )?;
        Ok(())
    }

    // ---- audit -----------------------------------------------------------

    pub fn insert_audit(&self, a: &AuditEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audit_records(run_id, timestamp, decision, rule, severity, file, agent_id, reason)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![a.run_id, a.timestamp, a.decision, a.rule, a.severity, a.file, a.agent_id, a.reason],
        )?;
        Ok(())
    }

    pub fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, timestamp, decision, rule, severity, file, agent_id, reason
             FROM audit_records ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(AuditEntry {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    timestamp: r.get(2)?,
                    decision: r.get(3)?,
                    rule: r.get(4)?,
                    severity: r.get(5)?,
                    file: r.get(6)?,
                    agent_id: r.get(7)?,
                    reason: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- tasks -----------------------------------------------------------

    pub fn insert_task(&self, t: &TaskRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tasks(id, created_at, kind, title, problem_json, source)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                t.id,
                t.created_at,
                t.kind,
                t.title,
                t.problem_json,
                t.source
            ],
        )?;
        Ok(())
    }

    pub fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, created_at, kind, title, problem_json, source FROM tasks ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TaskRecord {
                    id: r.get(0)?,
                    created_at: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    problem_json: r.get(4)?,
                    source: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- metrics ---------------------------------------------------------

    /// Dashboard için toplu sayaçlar (tek sorguda).
    pub fn metrics(&self) -> Result<Metrics> {
        let conn = self.conn.lock().unwrap();
        let count = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
        let float = |sql: &str| -> f64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0.0) };
        Ok(Metrics {
            runs_total: count("SELECT COUNT(*) FROM runs")?,
            runs_completed: count("SELECT COUNT(*) FROM runs WHERE status='completed'")?,
            runs_failed: count(
                "SELECT COUNT(*) FROM runs WHERE status IN ('failed','limit_reached','interrupted')",
            )?,
            evaluations_total: count("SELECT COUNT(*) FROM evaluations")?,
            guardian_blocks: count(
                "SELECT COUNT(*) FROM audit_records WHERE decision='blocked'",
            )?,
            llm_calls_total: count("SELECT COALESCE(SUM(total_llm_calls),0) FROM runs")?,
            best_score: float("SELECT COALESCE(MAX(best_score),0.0) FROM runs"),
            avg_score: float(
                "SELECT COALESCE(AVG(best_score),0.0) FROM runs WHERE status='completed'",
            ),
        })
    }

    // ---- retention -------------------------------------------------------

    /// Eski run'ları (ve bağlı kayıtlarını) budar. FK ON DELETE CASCADE ile
    /// ilişkili satırlar da silinir.
    pub fn prune(&self, policy: Retention) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM runs WHERE id IN (
                SELECT id FROM runs ORDER BY created_at DESC LIMIT -1 OFFSET ?1
             )",
            params![policy.max_runs as i64],
        )?;
        Ok(n)
    }
}

/// Dashboard özet sayaçları.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub runs_total: i64,
    pub runs_completed: i64,
    pub runs_failed: i64,
    pub evaluations_total: i64,
    pub guardian_blocks: i64,
    pub llm_calls_total: i64,
    pub best_score: f64,
    pub avg_score: f64,
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let status: String = r.get(4)?;
    let workload: String = r.get(5)?;
    Ok(RunRecord {
        id: r.get(0)?,
        created_at: r.get(1)?,
        started_at: r.get(2)?,
        finished_at: r.get(3)?,
        status: RunStatus::parse(&status).unwrap_or(RunStatus::Failed),
        workload: WorkloadKind::parse(&workload).unwrap_or(WorkloadKind::Production),
        problem_id: r.get(6)?,
        problem_title: r.get(7)?,
        model: r.get(8)?,
        temperature: r.get(9)?,
        sandbox: r.get(10)?,
        generation: r.get::<_, i64>(11)? as u32,
        iteration: r.get::<_, i64>(12)? as u32,
        best_score: r.get(13)?,
        best_agent_id: r.get(14)?,
        total_llm_calls: r.get::<_, i64>(15)? as u32,
        total_evaluations: r.get::<_, i64>(16)? as u32,
        production_success: r.get::<_, i64>(17)? != 0,
        error: r.get(18)?,
        quine_version: r.get(19)?,
        git_revision: r.get(20)?,
        sandbox_image: r.get(21)?,
    })
}

const RUN_COLS: &str = "id, created_at, started_at, finished_at, status, workload,
    problem_id, problem_title, model, temperature, sandbox,
    generation, iteration, best_score, best_agent_id,
    total_llm_calls, total_evaluations, production_success, error,
    quine_version, git_revision, sandbox_image";

const RUN_SELECT_BASE: &str = "SELECT id, created_at, started_at, finished_at, status, workload,
    problem_id, problem_title, model, temperature, sandbox,
    generation, iteration, best_score, best_agent_id,
    total_llm_calls, total_evaluations, production_success, error,
    quine_version, git_revision, sandbox_image FROM runs";

const RUN_SELECT_WHERE: &str = "SELECT id, created_at, started_at, finished_at, status, workload,
    problem_id, problem_title, model, temperature, sandbox,
    generation, iteration, best_score, best_agent_id,
    total_llm_calls, total_evaluations, production_success, error,
    quine_version, git_revision, sandbox_image FROM runs WHERE id=?1";

#[allow(dead_code)]
const _RUN_COLS_UNUSED: &str = RUN_COLS;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id                 TEXT PRIMARY KEY,
    created_at         TEXT NOT NULL,
    started_at         TEXT,
    finished_at        TEXT,
    status             TEXT NOT NULL,
    workload           TEXT NOT NULL,
    problem_id         TEXT NOT NULL,
    problem_title      TEXT NOT NULL,
    model              TEXT NOT NULL,
    temperature        REAL NOT NULL,
    sandbox            TEXT NOT NULL,
    generation         INTEGER NOT NULL DEFAULT 0,
    iteration          INTEGER NOT NULL DEFAULT 0,
    best_score         REAL NOT NULL DEFAULT 0.0,
    best_agent_id      TEXT,
    total_llm_calls    INTEGER NOT NULL DEFAULT 0,
    total_evaluations  INTEGER NOT NULL DEFAULT 0,
    production_success INTEGER NOT NULL DEFAULT 0,
    error              TEXT,
    quine_version      TEXT NOT NULL,
    git_revision       TEXT NOT NULL,
    sandbox_image      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_runs_created ON runs(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_runs_status  ON runs(status);

CREATE TABLE IF NOT EXISTS run_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    sequence    INTEGER NOT NULL,
    timestamp   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    agent_id    TEXT,
    generation  INTEGER,
    duration_ms INTEGER,
    payload     TEXT NOT NULL,
    UNIQUE(run_id, sequence)
);
CREATE INDEX IF NOT EXISTS idx_events_run ON run_events(run_id, sequence);

CREATE TABLE IF NOT EXISTS agents (
    id            TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    generation    INTEGER NOT NULL,
    parent_id     TEXT,
    prompt_hash   TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    fitness_score REAL NOT NULL DEFAULT 0.0,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agents_run ON agents(run_id, generation);

CREATE TABLE IF NOT EXISTS candidates (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    agent_id        TEXT NOT NULL,
    parent_id       TEXT,
    generation      INTEGER NOT NULL,
    prompt_hash     TEXT NOT NULL,
    code            TEXT NOT NULL,
    score           REAL NOT NULL,
    tests_passed    INTEGER NOT NULL,
    tests_total     INTEGER NOT NULL,
    duration_ms     INTEGER NOT NULL,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL,
    accepted        INTEGER NOT NULL DEFAULT 0,
    delta           REAL NOT NULL DEFAULT 0.0,
    mutation_reason TEXT,
    diff            TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_candidates_run ON candidates(run_id, generation);

CREATE TABLE IF NOT EXISTS evaluations (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id        TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    agent_id      TEXT NOT NULL,
    problem_id    TEXT NOT NULL,
    success       INTEGER NOT NULL,
    score         REAL NOT NULL,
    tests_passed  INTEGER NOT NULL,
    tests_total   INTEGER NOT NULL,
    duration_ms   INTEGER NOT NULL,
    stderr        TEXT NOT NULL DEFAULT '',
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_eval_run ON evaluations(run_id);

CREATE TABLE IF NOT EXISTS audit_records (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id    TEXT,
    timestamp TEXT NOT NULL,
    decision  TEXT NOT NULL,
    rule      TEXT,
    severity  TEXT,
    file      TEXT,
    agent_id  TEXT,
    reason    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_records(id DESC);

CREATE TABLE IF NOT EXISTS tasks (
    id           TEXT PRIMARY KEY,
    created_at   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    title        TEXT NOT NULL,
    problem_json TEXT NOT NULL,
    source       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS artifacts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    path       TEXT NOT NULL,
    bytes      INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_run ON artifacts(run_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run(id: &str) -> RunRecord {
        RunRecord {
            id: id.into(),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            status: RunStatus::Queued,
            workload: WorkloadKind::Production,
            problem_id: "fib-001".into(),
            problem_title: "Fibonacci".into(),
            model: "qwen2.5-coder:1.5b".into(),
            temperature: 0.0,
            sandbox: "docker".into(),
            generation: 0,
            iteration: 0,
            best_score: 0.0,
            best_agent_id: None,
            total_llm_calls: 0,
            total_evaluations: 0,
            production_success: false,
            error: None,
            quine_version: "0.1.0".into(),
            git_revision: "test".into(),
            sandbox_image: "rust:1-slim-bookworm".into(),
        }
    }

    #[test]
    fn schema_version_is_recorded() {
        let s = Store::open_in_memory().unwrap();
        let conn = s.conn.lock().unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM app_metadata WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn run_roundtrip_and_update() {
        let s = Store::open_in_memory().unwrap();
        let mut r = sample_run("run-1");
        s.insert_run(&r).unwrap();
        r.status = RunStatus::Running;
        r.total_llm_calls = 3;
        r.best_score = 75.0;
        s.update_run(&r).unwrap();
        let got = s.get_run("run-1").unwrap().unwrap();
        assert_eq!(got.status, RunStatus::Running);
        assert_eq!(got.total_llm_calls, 3);
        assert_eq!(got.best_score, 75.0);
    }

    #[test]
    fn events_get_monotonic_sequence() {
        let s = Store::open_in_memory().unwrap();
        s.insert_run(&sample_run("r")).unwrap();
        for kind in ["a", "b", "c"] {
            s.append_event(&EventRecord {
                run_id: "r".into(),
                sequence: 0,
                timestamp: Utc::now(),
                kind: kind.into(),
                agent_id: None,
                generation: Some(0),
                duration_ms: None,
                payload: serde_json::json!({}),
            })
            .unwrap();
        }
        let evs = s.list_events("r", 0).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(
            evs.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(s.list_events("r", 1).unwrap().len(), 2);
    }

    #[test]
    fn crash_recovery_marks_orphans_interrupted() {
        let s = Store::open_in_memory().unwrap();
        s.insert_run(&sample_run("live")).unwrap();
        let n = s.mark_orphans_interrupted().unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            s.get_run("live").unwrap().unwrap().status,
            RunStatus::Interrupted
        );
    }

    #[test]
    fn foreign_key_cascade_deletes_children() {
        let s = Store::open_in_memory().unwrap();
        s.insert_run(&sample_run("r")).unwrap();
        s.append_event(&EventRecord {
            run_id: "r".into(),
            sequence: 0,
            timestamp: Utc::now(),
            kind: "x".into(),
            agent_id: None,
            generation: None,
            duration_ms: None,
            payload: serde_json::json!({}),
        })
        .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute("DELETE FROM runs WHERE id='r'", []).unwrap();
        }
        assert!(s.list_events("r", 0).unwrap().is_empty());
    }

    #[test]
    fn retention_prunes_old_runs() {
        let s = Store::open_in_memory().unwrap();
        for i in 0..5 {
            let mut r = sample_run(&format!("r{i}"));
            r.created_at = Utc::now() + chrono::Duration::seconds(i as i64);
            s.insert_run(&r).unwrap();
        }
        let deleted = s.prune(Retention { max_runs: 2 }).unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(s.list_runs(10).unwrap().len(), 2);
    }

    #[test]
    fn metrics_aggregate() {
        let s = Store::open_in_memory().unwrap();
        let mut r = sample_run("r");
        r.status = RunStatus::Completed;
        r.best_score = 100.0;
        r.total_llm_calls = 4;
        s.insert_run(&r).unwrap();
        let m = s.metrics().unwrap();
        assert_eq!(m.runs_total, 1);
        assert_eq!(m.runs_completed, 1);
        assert_eq!(m.best_score, 100.0);
    }

    #[test]
    fn open_fails_closed_on_unwritable_path() {
        // Dizin yerine dosya yolu verilirse hata dönmeli (sessiz fallback yok).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let res = Store::open(file.join("db.sqlite"));
        assert!(res.is_err());
    }
}
