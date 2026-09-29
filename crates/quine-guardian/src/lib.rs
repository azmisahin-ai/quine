//! # quine-guardian
//!
//! Çekirdek Prensip #5: Ajanın ürettiği kod ana sisteme asla doğrudan
//! erişemez. [`DiffAnalyzer`] LLM çıktısındaki (veya kod diff'indeki)
//! tehlikeli pattern'leri tarar; ihlaller [`SecurityViolation`] ile
//! reddedilir ve isteğe bağlı olarak `data/audit.log` dosyasına yazılır.

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Tespit edilen tek bir tehlikeli desen bilgisi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Desenin kısa adı (örn. "unsafe-block").
    pub rule: String,
    /// İhlalin ciddiyeti.
    pub severity: Severity,
    /// Kaçıncı satırda bulunduğuna dair ipucu.
    pub line_number: usize,
    /// İhlal içeren satırın içeriği (ilk 120 karakterle sınırlı).
    pub excerpt: String,
}

/// İhlal ciddiyet seviyesi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Warning,
    Critical,
}

/// Guardian'ın red kararı (Faz 4 kabul kriteri bu hatayı bekler).
#[derive(Debug, Error, Clone, PartialEq, Serialize, Deserialize)]
#[error("SecurityViolation: {} kritik ihlal tespit edildi -> {:?}", violations.len(), first_excerpt(violations))]
pub struct SecurityViolation {
    /// Tüm bulgular.
    pub violations: Vec<Finding>,
}

fn first_excerpt(v: &[Finding]) -> String {
    v.first().map(|f| f.excerpt.clone()).unwrap_or_default()
}

/// Bir yasaklı desen tanımı.
#[derive(Debug, Clone)]
pub struct PatternRule {
    pub name: &'static str,
    pub regex: &'static str,
    pub severity: Severity,
    pub reason: &'static str,
}

/// Varsayılan yasaklı desen listesi (MASTER_PLAN Faz 4 / Adım 4.1).
pub const DEFAULT_RULES: &[PatternRule] = &[
    PatternRule {
        name: "unsafe-block",
        regex: r"\bunsafe\b",
        severity: Severity::Critical,
        reason: "`unsafe` blokları bellek güvenliğini imha eder.",
    },
    PatternRule {
        name: "fs-remove-all",
        regex: r"remove_dir_all|remove_file|\bfs::\s*remove",
        severity: Severity::Critical,
        reason: "Dosya/silme API'leri ajan kodunda yasaktır.",
    },
    PatternRule {
        name: "process-command",
        regex: r"std::process::Command|process::Command",
        severity: Severity::Critical,
        reason: "Alt süreç başlatmak yalnız sandbox runner'a aittir.",
    },
    PatternRule {
        name: "shell-rm-rf",
        regex: r"rm\s+-rf|mkfs\.|\bdd\s+if=",
        severity: Severity::Critical,
        reason: "Shell yıkım komutları (`rm -rf` vb.) kesinlikle yasaktır.",
    },
    PatternRule {
        name: "raw-network",
        regex: r"TcpStream|UdpSocket|net::TcpListener",
        severity: Severity::Warning,
        reason: "Ağ erişimi air-gapped ilkesini ihlal eder.",
    },
    PatternRule {
        name: "env-secret-access",
        regex: r"std::env|env::var",
        severity: Severity::Warning,
        reason: "Ortam değişkenleri gizli anahtar taşıyabilir.",
    },
];

/// Diff/kod analiz motoru.
#[derive(Debug, Clone)]
pub struct DiffAnalyzer {
    rules: Vec<(PatternRule, Regex)>,
    /// Yalnız diff satırlarını mı tara (`+` ile başlayanlar)?
    diff_only_added_lines: bool,
}

impl Default for DiffAnalyzer {
    fn default() -> Self {
        Self::new(DEFAULT_RULES.to_vec())
    }
}

impl DiffAnalyzer {
    pub fn new(rules: Vec<PatternRule>) -> Self {
        let compiled = rules
            .into_iter()
            .map(|r| {
                let re = Regex::new(r.regex)
                    .unwrap_or_else(|e| panic!("geçersiz regex '{}': {e}", r.regex));
                (r, re)
            })
            .collect();
        Self {
            rules: compiled,
            diff_only_added_lines: false,
        }
    }

    /// Unified-diff modu: yalnızca eklenen (`+`) satırlar taranır.
    pub fn with_diff_mode(mut self) -> Self {
        self.diff_only_added_lines = true;
        self
    }

    /// Kodu/diff'i tarar; kritik bulgu varsa `Err(SecurityViolation)` döner.
    /// Uyarılar (`Warning`) sonucu geçmez ama [`Self::scan`] üzerinden görülebilir.
    pub fn analyze(&self, code: &str) -> Result<(), SecurityViolation> {
        let findings = self.scan(code);
        let critical: Vec<Finding> = findings
            .into_iter()
            .filter(|f| f.severity == Severity::Critical)
            .collect();
        if critical.is_empty() {
            Ok(())
        } else {
            Err(SecurityViolation {
                violations: critical,
            })
        }
    }

    /// Tüm bulguları (uyarı + kritik) döner.
    pub fn scan(&self, code: &str) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (idx, raw_line) in code.lines().enumerate() {
            let line = if self.diff_only_added_lines {
                // Diff modunda context/remove satırlarını atla.
                match raw_line.strip_prefix('+') {
                    Some(l) => l,
                    None => continue,
                }
            } else {
                raw_line
            };
            // Yorum satırlarını atla: eğitim amaçlı yorumlar ihlal sayılmaz.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with('#') {
                continue;
            }
            for (rule, re) in &self.rules {
                if re.is_match(line) {
                    findings.push(Finding {
                        rule: rule.name.to_string(),
                        severity: rule.severity,
                        line_number: idx + 1,
                        excerpt: line.chars().take(120).collect(),
                    });
                }
            }
        }
        findings
    }
}

/// Denetim kaydı satırı (Faz 4: her karar data/audit.log'a yazılır).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp: DateTime<Utc>,
    pub decision: AuditDecision,
    pub agent_id: Option<String>,
    pub detail: String,
}

/// Verilen karar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Allowed,
    Blocked,
}

/// JSON-lines formatında `data/audit.log` yazıcı.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: std::path::PathBuf,
}

impl AuditLog {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Varsayılan konum: `data/audit.log`.
    pub fn default_location() -> Self {
        Self::new("data/audit.log")
    }

    /// Kaydı append eder. Dosyanın bulunduğu dizini oluşturur.
    pub fn record(
        &self,
        decision: AuditDecision,
        agent_id: Option<&str>,
        detail: &str,
    ) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rec = AuditRecord {
            timestamp: Utc::now(),
            decision,
            agent_id: agent_id.map(|s| s.to_string()),
            detail: detail.to_string(),
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(
            f,
            "{}",
            serde_json::to_string(&rec).expect("audit record serializes")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_code_passes() {
        let analyzer = DiffAnalyzer::default();
        let code = "pub fn fibonacci(n: u32) -> u64 {\n    let (mut a, mut b) = (0, 1);\n    for _ in 0..n { let t = a + b; a = b; b = t; }\n    a\n}";
        assert!(analyzer.analyze(code).is_ok());
    }

    #[test]
    fn detects_unsafe_block() {
        let analyzer = DiffAnalyzer::default();
        let err = analyzer
            .analyze("unsafe {\n    std::ptr::null_mut();\n}")
            .expect_err("must block unsafe");
        assert!(err.violations.iter().any(|f| f.rule == "unsafe-block"));
    }

    #[test]
    fn detects_remove_dir_all_and_command() {
        let analyzer = DiffAnalyzer::default();
        let err = analyzer
            .analyze("std::fs::remove_dir_all(\"/\").unwrap();\nstd::process::Command::new(\"sh\").spawn();")
            .expect_err("must block destructive fs/process calls");
        let rules: Vec<&str> = err.violations.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"fs-remove-all"));
        assert!(rules.contains(&"process-command"));
    }

    #[test]
    fn detects_rm_rf_in_shell_strings() {
        // Faz 4 kabul kriteri: ajan kasıtlı `rm -rf /` üretirse engellenmeli.
        let analyzer = DiffAnalyzer::default();
        let err = analyzer
            .analyze("run(\"rm -rf /\");")
            .expect_err("must block rm -rf");
        assert_eq!(err.violations[0].rule, "shell-rm-rf");
    }

    #[test]
    fn comments_do_not_trigger_findings() {
        let analyzer = DiffAnalyzer::default();
        assert!(analyzer
            .analyze("// bu bir açıklama: unsafe kullanmayın\nfn ok() {}")
            .is_ok());
    }

    #[test]
    fn diff_mode_scans_only_added_lines() {
        let analyzer = DiffAnalyzer::default().with_diff_mode();
        let diff = "--- a/x.rs\n+++ b/x.rs\n-let old = 1;\n+unsafe { core::mem::zeroed() }";
        let err = analyzer
            .analyze(diff)
            .expect_err("added unsafe line blocked");
        assert_eq!(err.violations.len(), 1);
        assert_eq!(err.violations[0].rule, "unsafe-block");
    }

    #[test]
    fn warnings_are_reported_by_scan_but_do_not_block() {
        let analyzer = DiffAnalyzer::default();
        let code = "let s = std::net::TcpStream::connect(\"127.0.0.1:80\")?;";
        let findings = analyzer.scan(code);
        assert!(findings.iter().any(|f| f.severity == Severity::Warning));
        // Warning tek başına engellemez:
        assert!(analyzer.analyze(code).is_ok());
    }

    #[test]
    fn audit_log_writes_jsonlines() {
        let dir = std::env::temp_dir().join(format!("quine-audit-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = AuditLog::new(dir.join("audit.log"));
        log.record(AuditDecision::Blocked, Some("agent-1"), "unsafe detected")
            .unwrap();
        log.record(AuditDecision::Allowed, None, "clean diff")
            .unwrap();
        let content = std::fs::read_to_string(dir.join("audit.log")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let rec: AuditRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(rec.decision, AuditDecision::Blocked);
        assert_eq!(rec.agent_id.as_deref(), Some("agent-1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
