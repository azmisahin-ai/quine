//! # quine-runtime
//!
//! Quine'ın **uygulama servis katmanı**: `Run` domain nesnesi, açık durum
//! makinesi, merkezi event bus, iptal (cancellation) ve run engine.
//!
//! Hem CLI hem web control plane **aynı** bu katmanı kullanır; mantık tekrarı
//! yoktur (bkz. `docs/ARCHITECTURE.md`).

pub mod engine;
pub mod event;
pub mod run;

pub use engine::{
    status_label, version_string, RunConfig, RunControl, RunEngine, RunHandle, RunLimits, RunMode,
    RunOutcome, RunRequest, RunSummary, RuntimeContext,
};
pub use event::{EventBus, RunEvent, RunEventKind};
pub use run::{Run, RunError};

/// Kütüphane sürümü (run metadata'sına yazılır).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Çalışma zamanı ortam parmak izi (reproducibility için).
#[derive(Debug, Clone, Default)]
pub struct BuildInfo {
    pub version: String,
    pub git_revision: String,
}

impl BuildInfo {
    /// Ortamdan build bilgisi toplar (`QUINE_GIT_REVISION` veya "unknown").
    pub fn detect() -> Self {
        Self {
            version: VERSION.to_string(),
            git_revision: std::env::var("QUINE_GIT_REVISION").unwrap_or_else(|_| "unknown".into()),
        }
    }
}
