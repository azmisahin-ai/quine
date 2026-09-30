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
    // ---- Bellek güvenliği / FFI ------------------------------------------
    PatternRule {
        name: "unsafe-block",
        regex: r"\bunsafe\b",
        severity: Severity::Critical,
        reason: "`unsafe` blokları bellek güvenliğini imha eder.",
    },
    PatternRule {
        name: "extern-ffi",
        regex: r#"\bextern\s*"C"|\blibc\s*::"#,
        severity: Severity::Critical,
        reason: "FFI/libc çağrıları sandbox sınırını aşabilir.",
    },
    // ---- Dosya sistemi: hem okuma hem yazma yasak ------------------------
    PatternRule {
        name: "fs-module",
        regex: r"\bfs\s*::",
        severity: Severity::Critical,
        reason: "Dosya sistemi API'leri (std::fs) ajan kodunda yasaktır.",
    },
    PatternRule {
        name: "fs-file-open",
        regex: r"\bFile\s*::\s*(create|open|create_new|options)|\bOpenOptions\b",
        severity: Severity::Critical,
        reason: "Dosya açma/oluşturma ajan kodunda yasaktır.",
    },
    PatternRule {
        name: "fs-mutate",
        regex: r"\b(create_dir|create_dir_all|rename|copy|hard_link|soft_link|symlink|set_permissions|set_len|truncate)\s*\(",
        severity: Severity::Critical,
        reason: "Dosya sistemi değiştiren çağrılar yasaktır.",
    },
    PatternRule {
        name: "fs-remove",
        regex: r"\b(remove_dir_all|remove_dir|remove_file)\s*\(",
        severity: Severity::Critical,
        reason: "Dosya/silme API'leri ajan kodunda yasaktır.",
    },
    PatternRule {
        name: "absolute-path",
        regex: r#""/[A-Za-z][^"]*""#,
        severity: Severity::Critical,
        reason: "Mutlak sistem yollarına erişim yasaktır.",
    },
    PatternRule {
        name: "windows-absolute-path",
        regex: r"[A-Za-z]:\\",
        severity: Severity::Critical,
        reason: "Windows mutlak yollarına erişim yasaktır.",
    },
    PatternRule {
        name: "path-traversal",
        regex: r"\.\./",
        severity: Severity::Critical,
        reason: "`../` yol geçişi (path traversal) yasaktır.",
    },
    // ---- Ortam değişkenleri: gizli anahtar sızıntısı ---------------------
    PatternRule {
        name: "env-access",
        regex: r"\benv\s*::|\bstd::env\b|\benv!|option_env!",
        severity: Severity::Critical,
        reason: "Ortam değişkenleri gizli anahtar taşır; erişim yasaktır.",
    },
    // ---- Süreç / shell ---------------------------------------------------
    PatternRule {
        name: "process-command",
        regex: r"\bstd::process\b|\bprocess\s*::|\bCommand\s*::\s*new\b",
        severity: Severity::Critical,
        reason: "Alt süreç başlatmak yalnız sandbox runner'a aittir.",
    },
    PatternRule {
        name: "shell-destructive",
        regex: r"rm\s+-rf|mkfs\.|\bdd\s+if=|:\(\)\s*\{",
        severity: Severity::Critical,
        reason: "Shell yıkım komutları (`rm -rf` vb.) kesinlikle yasaktır.",
    },
    // ---- Ağ --------------------------------------------------------------
    PatternRule {
        name: "network-access",
        regex: r"TcpStream|UdpSocket|TcpListener|\bstd::net\b|reqwest|hyper|\bureq\b",
        severity: Severity::Critical,
        reason: "Ağ erişimi air-gapped ilkesini ihlal eder.",
    },
    // ---- Derleme zamanında host dosyası okuma ---------------------------
    PatternRule {
        name: "include-host-file",
        regex: r"\binclude_str!|\binclude_bytes!|\binclude\s*!\s*\(",
        severity: Severity::Critical,
        reason: "Derleme zamanında host dosyası okumak yasaktır.",
    },
    // ---- Platforma özel kaçış -------------------------------------------
    PatternRule {
        name: "os-specific",
        regex: r"\bstd::os\b",
        severity: Severity::Critical,
        reason: "Platforma özel API'ler sandbox sınırını aşabilir.",
    },
    // ---- Takma adlı içe aktarma (alias) kaçışı ---------------------------
    //
    // `use std::fs as f; f::write(...)` biçiminde bir takma ad, `fs::`
    // desenini atlatır ve modül taraması boşa düşer. Kritik modüllerin
    // takma adla içe aktarılmasını doğrudan engelliyoruz.
    PatternRule {
        name: "aliased-module-import",
        regex: r"\buse\s+(std|core|alloc)\s*::\s*\{?\s*(fs|process|env|net|os|path)\b",
        severity: Severity::Critical,
        reason: "Tehlikeli modüllerin takma adla içe aktarılması yasaktır.",
    },
    // ---- Yol (path) modülü: traversal'ın ikinci kapısı -------------------
    PatternRule {
        name: "path-module",
        regex: r"\bstd\s*::\s*path\b|\bPath\s*::\s*(new|from)\b|\bPathBuf\b",
        severity: Severity::Critical,
        reason: "Yol manipülasyonu sandbox dışına çıkmak için kullanılabilir.",
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
        assert!(rules.contains(&"fs-remove"), "{rules:?}");
        assert!(rules.contains(&"process-command"), "{rules:?}");
    }

    #[test]
    fn detects_rm_rf_in_shell_strings() {
        // Faz 4 kabul kriteri: ajan kasıtlı `rm -rf /` üretirse engellenmeli.
        let analyzer = DiffAnalyzer::default();
        let err = analyzer
            .analyze("run(\"rm -rf /\");")
            .expect_err("must block rm -rf");
        let rules: Vec<&str> = err.violations.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"shell-destructive"), "{rules:?}");
    }

    /// P0 regresyon: host dosya sistemine yazma denemesi engellenmeli.
    #[test]
    fn blocks_host_filesystem_write_attempts() {
        let analyzer = DiffAnalyzer::default();
        let payloads = [
            r#"use std::fs; pub fn f() { fs::write("/tmp/QUINE_ESCAPE_PROOF.txt", "owned").ok(); }"#,
            r#"use std::fs::File; pub fn f() { File::create("/tmp/x").ok(); }"#,
            r#"use std::fs::OpenOptions; pub fn f() { OpenOptions::new().write(true).open("/tmp/x").ok(); }"#,
            r#"pub fn f() { std::fs::create_dir_all("/tmp/evil").ok(); }"#,
            r#"pub fn f() { std::fs::rename("/a", "/b").ok(); }"#,
            r#"pub fn f() { std::fs::copy("/a", "/b").ok(); }"#,
            r#"pub fn f() { std::fs::remove_file("/etc/passwd").ok(); }"#,
            r#"pub fn f() { let _ = std::fs::read_to_string("/etc/shadow"); }"#,
            r#"pub fn f() { let _ = std::fs::read("../../secret"); }"#,
        ];
        for p in payloads {
            assert!(
                analyzer.analyze(p).is_err(),
                "P0: bu payload engellenmeliydi: {p}"
            );
        }
    }

    /// P0 regresyon: ortam değişkeni okuma artık kritik.
    #[test]
    fn blocks_environment_access() {
        let analyzer = DiffAnalyzer::default();
        for p in [
            r#"pub fn f() -> String { std::env::var("GITHUB_TOKEN").unwrap_or_default() }"#,
            r#"pub fn f() -> Option<String> { std::env::var("AWS_SECRET_ACCESS_KEY").ok() }"#,
        ] {
            let err = analyzer.analyze(p).expect_err("env erişimi engellenmeli");
            assert!(
                err.violations.iter().any(|f| f.rule == "env-access"),
                "{:?}",
                err.violations
            );
        }
    }

    /// P0 regresyon: mutlak yol ve `../` traversal kritik.
    #[test]
    fn blocks_absolute_paths_and_traversal() {
        let analyzer = DiffAnalyzer::default();
        for p in [
            r#"pub fn f() -> String { read("/etc/hosts") }"#,
            r#"pub fn f() -> String { read("/root/.ssh/id_rsa") }"#,
            r#"pub fn f() -> String { read("../../../etc/passwd") }"#,
            r#"pub fn f() -> String { read("C:\\Users\\admin\\secrets.txt") }"#,
        ] {
            assert!(analyzer.analyze(p).is_err(), "engellenmeliydi: {p}");
        }
    }

    /// P0 regresyon: ağ erişimi artık kritik.
    #[test]
    fn blocks_network_access() {
        let analyzer = DiffAnalyzer::default();
        let err = analyzer
            .analyze("let s = std::net::TcpStream::connect(\"127.0.0.1:80\")?;")
            .expect_err("ağ erişimi engellenmeli");
        assert!(err.violations.iter().any(|f| f.rule == "network-access"));
    }

    /// Temiz, zararsız kod hâlâ geçmeli (aşırı katılık regresyonu).
    #[test]
    fn normal_solution_code_still_passes() {
        let analyzer = DiffAnalyzer::default();
        let samples = [
            "pub fn fibonacci(n: u32) -> u64 {\n    let (mut a, mut b) = (0u64, 1u64);\n    for _ in 0..n { let t = a + b; a = b; b = t; }\n    a\n}",
            "pub fn reverse(s: &str) -> String { s.chars().rev().collect() }",
            "pub fn sum(nums: &[i64]) -> i64 { nums.iter().sum() }",
            "pub fn title_case(s: &str) -> String {\n    s.split_whitespace().map(|w| {\n        let mut c = w.chars();\n        c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default()\n    }).collect::<Vec<_>>().join(\" \")\n}",
            "pub fn second_max(nums: &[i64]) -> i64 {\n    let mut v: Vec<i64> = nums.to_vec();\n    v.sort();\n    v.iter().rev().nth(1).copied().unwrap_or(-1)\n}",
        ];
        for s in samples {
            assert!(analyzer.analyze(s).is_ok(), "temiz kod engellendi: {s}");
        }
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
        // Kritik olmayan bulgular `scan` ile görünür ama tek başına engellemez.
        let warn_rule = PatternRule {
            name: "test-warning",
            regex: r"\bTODO\b",
            severity: Severity::Warning,
            reason: "test uyarısı",
        };
        let analyzer = DiffAnalyzer::new(vec![warn_rule]);
        let code = "let x = 1; // TODO: iyileştir";
        let findings = analyzer.scan(code);
        assert!(findings.iter().any(|f| f.severity == Severity::Warning));
        assert!(analyzer.analyze(code).is_ok());
    }

    #[test]
    fn default_rules_are_all_critical() {
        // P0: varsayılan politika fail-closed olmalı — hiçbir kural Warning değil.
        assert!(
            DEFAULT_RULES.iter().all(|r| r.severity == Severity::Critical),
            "tüm varsayılan kurallar kritik olmalı"
        );
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

    /// P0: Adversarial saldırı paketi — kaçış denemeleri engellenmeli.
    ///
    /// Her örnek, ajanın ürettiği kodu ana sisteme ulaştırmayı hedefleyen
    /// gerçekçi bir kaçış denemesidir. Hiçbiri `analyze`'dan geçememeli.
    #[test]
    fn adversarial_escapes_are_blocked() {
        let analyzer = DiffAnalyzer::default();
        let attacks: &[(&str, &str)] = &[
            ("host dosyası okuma", r#"let s = std::fs::read_to_string("/etc/passwd").unwrap();"#),
            ("host dosyasına yazma", r#"std::fs::write("/tmp/pwned", "x").unwrap();"#),
            ("takma adlı fs", r#"use std::fs as f; f::write("/etc/cron.d/x", "y").unwrap();"#),
            ("env sızıntısı", r#"let k = std::env::var("GITHUB_TOKEN").unwrap();"#),
            ("env! makro", r#"let k = env!("SECRET_KEY");"#),
            ("mutlak yol", r#"let p = "/root/.ssh/id_rsa";"#),
            ("path traversal", r#"let p = "../../../etc/shadow";"#),
            ("alt süreç", r#"std::process::Command::new("sh").arg("-c").arg("id").output();"#),
            ("shell yıkım", r#"let c = "rm -rf /";"#),
            ("ağ erişimi", r#"let s = std::net::TcpStream::connect("10.0.0.1:22");"#),
            ("unsafe bellek", r#"let v: u64 = unsafe { core::mem::zeroed() };"#),
            ("FFI/libc", r#"extern "C" { fn system(c: *const u8) -> i32; }"#),
            ("derleme zamanı host okuma", r#"let x = include_str!("/etc/hostname");"#),
            ("platform kaçışı", r#"use std::os::unix::fs::PermissionsExt;"#),
            ("PathBuf ile kaçış", r#"let p = PathBuf::from("/etc/shadow");"#),
        ];
        for (name, code) in attacks {
            let r = analyzer.analyze(code);
            assert!(
                r.is_err(),
                "saldırı engellenmedi ({name}): {code}\n-> {r:?}"
            );
        }
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
