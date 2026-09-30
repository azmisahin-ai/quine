//! `Run` — birinci sınıf domain nesnesi ve **açık durum makinesi**.
//!
//! Geçersiz durum geçişleri engellenir; böylece "tamamlanmış bir run yeniden
//! çalışıyor" veya "iptal edilmiş bir run tamamlandı" gibi tutarsız durumlar
//! oluşamaz.

use chrono::{DateTime, Utc};
use quine_storage::{RunRecord, RunStatus, WorkloadKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Geçersiz bir durum geçişi denendi.
#[derive(Debug, Error, Clone, PartialEq)]
#[error("geçersiz run durum geçişi: {from:?} → {to:?}")]
pub struct RunError {
    pub from: RunStatus,
    pub to: RunStatus,
}

/// Bir çalıştırmanın tüm çalışma zamanı durumu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
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
}

impl Run {
    /// Yeni bir run'ı `Queued` durumunda oluşturur.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        workload: WorkloadKind,
        problem_id: impl Into<String>,
        problem_title: impl Into<String>,
        model: impl Into<String>,
        temperature: f64,
        sandbox: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            status: RunStatus::Queued,
            workload,
            problem_id: problem_id.into(),
            problem_title: problem_title.into(),
            model: model.into(),
            temperature,
            sandbox: sandbox.into(),
            generation: 0,
            iteration: 0,
            best_score: 0.0,
            best_agent_id: None,
            total_llm_calls: 0,
            total_evaluations: 0,
            production_success: false,
            error: None,
        }
    }

    /// Durum geçişinin geçerli olup olmadığını belirler.
    ///
    /// Kurallar:
    /// * `queued` → `running` | `cancelled` | `failed`
    /// * `running` → `paused` | `cancelling` | `completed` | `failed` | `limit_reached` | `interrupted`
    /// * `paused` → `running` | `cancelling` | `failed`
    /// * `cancelling` → `cancelled` | `failed`
    /// * terminal durumlardan çıkış yok.
    pub fn can_transition(from: RunStatus, to: RunStatus) -> bool {
        use RunStatus::*;
        if from == to {
            return true; // idempotent güncelleme
        }
        matches!(
            (from, to),
            (Queued, Running)
                | (Queued, Cancelled)
                | (Queued, Failed)
                | (Running, Paused)
                | (Running, Cancelling)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, LimitReached)
                | (Running, Interrupted)
                | (Paused, Running)
                | (Paused, Cancelling)
                | (Paused, Failed)
                | (Paused, LimitReached)
                | (Cancelling, Cancelled)
                | (Cancelling, Failed)
        )
    }

    /// Geçişi uygular; geçersizse [`RunError`] döner.
    pub fn transition(&mut self, to: RunStatus) -> Result<(), RunError> {
        if !Self::can_transition(self.status, to) {
            return Err(RunError {
                from: self.status,
                to,
            });
        }
        self.status = to;
        match to {
            RunStatus::Running if self.started_at.is_none() => self.started_at = Some(Utc::now()),
            RunStatus::Completed
            | RunStatus::Failed
            | RunStatus::Cancelled
            | RunStatus::Interrupted
            | RunStatus::LimitReached => self.finished_at = Some(Utc::now()),
            _ => {}
        }
        Ok(())
    }

    /// Terminal (artık değişmeyecek) durumda mı?
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RunStatus::Completed
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::Interrupted
                | RunStatus::LimitReached
        )
    }

    /// Kalıcı kayıt biçimine çevirir.
    pub fn to_record(&self, build: &super::BuildInfo, sandbox_image: &str) -> RunRecord {
        RunRecord {
            id: self.id.clone(),
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            status: self.status,
            workload: self.workload,
            problem_id: self.problem_id.clone(),
            problem_title: self.problem_title.clone(),
            model: self.model.clone(),
            temperature: self.temperature,
            sandbox: self.sandbox.clone(),
            generation: self.generation,
            iteration: self.iteration,
            best_score: self.best_score,
            best_agent_id: self.best_agent_id.clone(),
            total_llm_calls: self.total_llm_calls,
            total_evaluations: self.total_evaluations,
            production_success: self.production_success,
            error: self.error.clone(),
            quine_version: build.version.clone(),
            git_revision: build.git_revision.clone(),
            sandbox_image: sandbox_image.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> Run {
        Run::new(
            "r1",
            WorkloadKind::Production,
            "fib-001",
            "Fibonacci",
            "m",
            0.0,
            "docker",
        )
    }

    #[test]
    fn valid_lifecycle_transitions() {
        let mut r = mk();
        r.transition(RunStatus::Running).unwrap();
        assert!(r.started_at.is_some());
        r.transition(RunStatus::Paused).unwrap();
        r.transition(RunStatus::Running).unwrap();
        r.transition(RunStatus::Completed).unwrap();
        assert!(r.finished_at.is_some());
        assert!(r.is_terminal());
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        let mut r = mk();
        // queued → completed geçersiz (önce running olmalı).
        assert!(r.transition(RunStatus::Completed).is_err());
        r.transition(RunStatus::Running).unwrap();
        assert!(r.transition(RunStatus::Cancelled).is_err()); // önce cancelling
        r.transition(RunStatus::Cancelling).unwrap();
        r.transition(RunStatus::Cancelled).unwrap();
        // Terminal durumdan çıkış yok.
        assert!(r.transition(RunStatus::Running).is_err());
    }

    #[test]
    fn cancel_from_queued_is_allowed() {
        let mut r = mk();
        r.transition(RunStatus::Cancelled).unwrap();
        assert_eq!(r.status, RunStatus::Cancelled);
    }

    #[test]
    fn limit_reached_is_terminal() {
        let mut r = mk();
        r.transition(RunStatus::Running).unwrap();
        r.transition(RunStatus::LimitReached).unwrap();
        assert!(r.is_terminal());
    }
}
