//! Merkezi event modeli: her yaşam döngüsü adımı bir olay üretir.
//!
//! Olaylar üç yere gider (bkz. MASTER_PLAN §8):
//! 1. **persist** edilir (SQLite `run_events`),
//! 2. UI'ya **publish** edilir (tokio broadcast → SSE),
//! 3. `tracing` ile **log**lanır.

use chrono::{DateTime, Utc};
use quine_storage::{EventRecord, Store};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Run yaşam döngüsündeki olay türleri.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunEventKind {
    RunStarted,
    ProblemLoaded,
    LlmRequestStarted,
    LlmResponseReceived,
    CodeExtracted,
    GuardianAllowed,
    GuardianBlocked,
    SandboxStarted,
    SandboxCompleted,
    CompilationFailed,
    TestsStarted,
    TestsCompleted,
    EvaluationCompleted,
    MutationProposed,
    MutationRejected,
    MutationAccepted,
    CandidateCreated,
    GenerationCompleted,
    RunPaused,
    RunResumed,
    RunCancelling,
    RunCompleted,
    RunFailed,
    RunCancelled,
    LimitReached,
}

impl RunEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunEventKind::RunStarted => "RUN_STARTED",
            RunEventKind::ProblemLoaded => "PROBLEM_LOADED",
            RunEventKind::LlmRequestStarted => "LLM_REQUEST_STARTED",
            RunEventKind::LlmResponseReceived => "LLM_RESPONSE_RECEIVED",
            RunEventKind::CodeExtracted => "CODE_EXTRACTED",
            RunEventKind::GuardianAllowed => "GUARDIAN_ALLOWED",
            RunEventKind::GuardianBlocked => "GUARDIAN_BLOCKED",
            RunEventKind::SandboxStarted => "SANDBOX_STARTED",
            RunEventKind::SandboxCompleted => "SANDBOX_COMPLETED",
            RunEventKind::CompilationFailed => "COMPILATION_FAILED",
            RunEventKind::TestsStarted => "TESTS_STARTED",
            RunEventKind::TestsCompleted => "TESTS_COMPLETED",
            RunEventKind::EvaluationCompleted => "EVALUATION_COMPLETED",
            RunEventKind::MutationProposed => "MUTATION_PROPOSED",
            RunEventKind::MutationRejected => "MUTATION_REJECTED",
            RunEventKind::MutationAccepted => "MUTATION_ACCEPTED",
            RunEventKind::CandidateCreated => "CANDIDATE_CREATED",
            RunEventKind::GenerationCompleted => "GENERATION_COMPLETED",
            RunEventKind::RunPaused => "RUN_PAUSED",
            RunEventKind::RunResumed => "RUN_RESUMED",
            RunEventKind::RunCancelling => "RUN_CANCELLING",
            RunEventKind::RunCompleted => "RUN_COMPLETED",
            RunEventKind::RunFailed => "RUN_FAILED",
            RunEventKind::RunCancelled => "RUN_CANCELLED",
            RunEventKind::LimitReached => "LIMIT_REACHED",
        }
    }
}

/// Yayınlanan/ kalıcı hale getirilen tek bir olay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: String,
    pub sequence: i64,
    pub timestamp: DateTime<Utc>,
    pub kind: RunEventKind,
    pub agent_id: Option<String>,
    pub generation: Option<u32>,
    pub duration_ms: Option<u64>,
    pub payload: serde_json::Value,
}

/// Olayları persist eden ve abonelere yayan merkez.
#[derive(Clone)]
pub struct EventBus {
    store: Arc<Store>,
    tx: broadcast::Sender<RunEvent>,
}

impl EventBus {
    /// Yeni bus: verilen depoya yazar, `capacity` genişliğinde yayın kanalı açar.
    pub fn new(store: Arc<Store>, capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { store, tx }
    }

    /// Canlı akışa abone olur (SSE).
    pub fn subscribe(&self) -> broadcast::Receiver<RunEvent> {
        self.tx.subscribe()
    }

    /// Olayı kalıcı hale getirir, loglar ve yayınlar. Atanan sıra numarasını döner.
    ///
    /// Persist başarısız olursa hata döner (sessizce yutulmaz); yayın en iyi
    /// çaba iledir (abone yoksa zararsızdır).
    pub fn emit(
        &self,
        run_id: &str,
        kind: RunEventKind,
        agent_id: Option<String>,
        generation: Option<u32>,
        duration_ms: Option<u64>,
        payload: serde_json::Value,
    ) -> anyhow::Result<i64> {
        let rec = EventRecord {
            run_id: run_id.to_string(),
            sequence: 0,
            timestamp: Utc::now(),
            kind: kind.as_str().to_string(),
            agent_id: agent_id.clone(),
            generation,
            duration_ms,
            payload: payload.clone(),
        };
        let seq = self.store.append_event(&rec)?;
        tracing::info!(
            run_id = %run_id,
            seq = seq,
            kind = kind.as_str(),
            agent = agent_id.as_deref().unwrap_or("-"),
            "event"
        );
        let ev = RunEvent {
            run_id: run_id.to_string(),
            sequence: seq,
            timestamp: rec.timestamp,
            kind,
            agent_id,
            generation,
            duration_ms,
            payload,
        };
        let _ = self.tx.send(ev);
        Ok(seq)
    }

    /// Depodan geçmiş olayları okur (UI ilk yükleme / reconnect).
    pub fn history(&self, run_id: &str, after: i64) -> anyhow::Result<Vec<RunEvent>> {
        let recs = self.store.list_events(run_id, after)?;
        Ok(recs
            .into_iter()
            .map(|r| RunEvent {
                run_id: r.run_id,
                sequence: r.sequence,
                timestamp: r.timestamp,
                kind: parse_kind(&r.kind),
                agent_id: r.agent_id,
                generation: r.generation,
                duration_ms: r.duration_ms,
                payload: r.payload,
            })
            .collect())
    }
}

fn parse_kind(s: &str) -> RunEventKind {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(RunEventKind::EvaluationCompleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quine_storage::{RunRecord, RunStatus, WorkloadKind};

    fn seed_run(store: &Store, id: &str) {
        let rec = RunRecord {
            id: id.into(),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            status: RunStatus::Running,
            workload: WorkloadKind::Demo,
            problem_id: "p".into(),
            problem_title: "P".into(),
            model: "m".into(),
            temperature: 0.0,
            sandbox: "local".into(),
            generation: 0,
            iteration: 0,
            best_score: 0.0,
            best_agent_id: None,
            total_llm_calls: 0,
            total_evaluations: 0,
            production_success: false,
            error: None,
            quine_version: "0".into(),
            git_revision: "t".into(),
            sandbox_image: "img".into(),
        };
        store.insert_run(&rec).unwrap();
    }

    #[test]
    fn emit_persists_and_sequences() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_run(&store, "r");
        let bus = EventBus::new(store.clone(), 16);
        let s1 = bus
            .emit(
                "r",
                RunEventKind::RunStarted,
                None,
                None,
                None,
                serde_json::json!({}),
            )
            .unwrap();
        let s2 = bus
            .emit(
                "r",
                RunEventKind::LlmRequestStarted,
                None,
                Some(0),
                Some(5),
                serde_json::json!({"model":"m"}),
            )
            .unwrap();
        assert_eq!((s1, s2), (1, 2));
        let hist = bus.history("r", 0).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].kind, RunEventKind::RunStarted);
        assert_eq!(hist[1].generation, Some(0));
    }

    #[tokio::test]
    async fn subscribers_receive_events() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_run(&store, "r");
        let bus = EventBus::new(store, 16);
        let mut rx = bus.subscribe();
        bus.emit(
            "r",
            RunEventKind::RunStarted,
            None,
            None,
            None,
            serde_json::json!({}),
        )
        .unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.kind, RunEventKind::RunStarted);
    }
}
