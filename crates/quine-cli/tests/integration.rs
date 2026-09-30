//! Uçtan uca entegrasyon testleri (Faz 1–4).
//!
//! Bu testler crate'ler arası akışı gerçek kod yollarıyla doğrular:
//! `LlmBackend` → `extract_code` → `Evaluator` (Guardian + Sandbox) →
//! `Evolution` (Archive / PopulationManager). Ağa veya Ollama'ya ihtiyaç
//! duymaz; `EchoBackend` deterministik çıktı verir.

use std::sync::Arc;

use quine_common::{Agent, EvaluationResult};
use quine_eval::{Evaluator, LocalProcessSandbox, SandboxLimits};
use quine_evolution::{evaluate_population, Archive, ArchiveEntry, PopulationManager};
use quine_llm::{EchoBackend, LlmBackend, LlmRequest};

fn evaluator() -> Evaluator {
    Evaluator::new(Box::new(LocalProcessSandbox::new(SandboxLimits::default())))
}

#[tokio::test]
async fn evaluate_accepts_llm_fenced_code() {
    let problem = quine_bench_simple::SimpleBenchmark::problem("fib-001").expect("problem");
    let llm = EchoBackend::fibonacci_solver();
    let resp = llm
        .generate(&LlmRequest::new("echo", "sys", problem.to_llm_prompt()))
        .await
        .expect("generate");
    let code = resp.extract_code();

    let result: EvaluationResult = evaluator()
        .evaluate(uuid::Uuid::new_v4(), &problem, &code)
        .await;

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.score, 100.0);
    assert_eq!(result.tests_passed, result.tests_total);
}

#[tokio::test]
async fn guardian_blocks_dangerous_code_before_sandbox() {
    let problem = quine_bench_simple::SimpleBenchmark::problem("fib-001").expect("problem");
    let evil = "pub fn fibonacci(n: u32) -> u64 { unsafe { std::process::Command::new(\"rm\").status().unwrap(); } 0 }";

    let result = evaluator()
        .evaluate(uuid::Uuid::new_v4(), &problem, evil)
        .await;

    assert!(!result.success);
    assert_eq!(result.score, 0.0);
    assert!(
        result.stderr.contains("GUARDIAN BLOCKED"),
        "beklenen guardian bloğu: {}",
        result.stderr
    );
}

#[tokio::test]
async fn broken_code_scores_zero_not_panic() {
    let problem = quine_bench_simple::SimpleBenchmark::problem("fib-001").expect("problem");
    let result = evaluator()
        .evaluate(
            uuid::Uuid::new_v4(),
            &problem,
            "pub fn fibonacci(n: u32) -> u64 { nope }",
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.score, 0.0);
}

#[tokio::test]
async fn population_evaluation_updates_fitness() {
    let problem = quine_bench_simple::SimpleBenchmark::problem("fib-001").expect("problem");
    let llm: Arc<dyn LlmBackend> = Arc::new(EchoBackend::fibonacci_solver());
    let ev = Arc::new(evaluator());

    let population: Vec<Agent> = (0..3).map(|i| Agent::new(format!("agent-{i}"))).collect();

    let scored = evaluate_population(llm, ev, population, vec![problem], "echo".to_string()).await;

    assert_eq!(scored.len(), 3);
    for (agent, results) in scored {
        assert_eq!(results.len(), 1);
        assert_eq!(agent.fitness_score, 100.0);
        assert!(results[0].success);
    }
}

#[test]
fn archive_roundtrip_and_upsert_keeps_best() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::new(dir.path().join("archive.json"));

    assert!(archive.load().expect("empty load").is_empty());

    let mut low = Agent::new("low");
    low.fitness_score = 10.0;
    let mut high = Agent::new("high");
    high.fitness_score = 90.0;

    archive
        .upsert(
            ArchiveEntry {
                agent: low.clone(),
                best_score: 10.0,
                generations_survived: 1,
            },
            10,
        )
        .expect("upsert low");
    archive
        .upsert(
            ArchiveEntry {
                agent: high.clone(),
                best_score: 90.0,
                generations_survived: 2,
            },
            10,
        )
        .expect("upsert high");

    let entries = archive.load().expect("load");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].agent.id, high.id, "en iyi skor başta olmalı");

    // Aynı ajanı güncelle: yeni kayıt eklenmemeli.
    archive
        .upsert(
            ArchiveEntry {
                agent: high.clone(),
                best_score: 95.0,
                generations_survived: 3,
            },
            10,
        )
        .expect("upsert update");
    let entries = archive.load().expect("reload");
    assert_eq!(entries.len(), 2, "upsert mevcut ajanı güncellemeli");
    assert_eq!(entries[0].best_score, 95.0);
}

#[test]
fn population_manager_selects_and_breeds() {
    let manager = PopulationManager::new(4, 1);
    let mut agents: Vec<Agent> = (0..4).map(|i| Agent::new(format!("a{i}"))).collect();
    agents[0].fitness_score = 80.0;
    agents[1].fitness_score = 20.0;
    agents[2].fitness_score = 60.0;
    agents[3].fitness_score = 5.0;

    let parents = manager.select_parents_roulette(&agents, 2);
    assert_eq!(parents.len(), 2);

    let next = manager.next_generation(&agents, &parents);
    assert_eq!(next.len(), 4);
    // Elitizm: en yüksek fitness'lı ajan korunur.
    assert!(next.iter().any(|a| a.id == agents[0].id));
    // Çocuklar sonraki jenerasyona ait olmalı.
    assert!(next.iter().any(|a| a.generation == 1));
}

/// Docker sandbox izolasyon testi (opt-in).
///
/// Varsayılan olarak atlanır; CI'da çalıştırmak için:
/// `QUINE_TEST_DOCKER=1 cargo test -p quine-cli --test integration -- --ignored`
/// Docker yoksa sessizce geçer (geliştirici ortamını kırmaz).
#[tokio::test]
#[ignore = "docker gerektirir; QUINE_TEST_DOCKER=1 ile çalıştır"]
async fn docker_sandbox_evaluates_in_isolation() {
    if std::env::var("QUINE_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!("QUINE_TEST_DOCKER=1 değil — atlanıyor");
        return;
    }
    if !quine_eval::DockerSandbox::available() {
        eprintln!("docker yok — atlanıyor");
        return;
    }

    let problem = quine_bench_simple::SimpleBenchmark::problem("fib-001").expect("problem");
    let ev = Evaluator::new(Box::new(quine_eval::DockerSandbox::default()));
    let code = EchoBackend::fibonacci_solver()
        .generate(&LlmRequest::new("echo", "sys", problem.to_llm_prompt()))
        .await
        .expect("generate")
        .extract_code();

    let result = ev.evaluate(uuid::Uuid::new_v4(), &problem, &code).await;
    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.score, 100.0);
}

/// Faz 4 / Adım 4.3: `CodeMutator` gerçek bir dosyayı değiştirir, yedeği alır;
/// guardian kritik ihlalde ise dosyayı **dokunmadan** bırakır ve engeli
/// `data/audit.log`'a yazar.
#[tokio::test]
async fn code_mutation_is_applied_and_guarded() {
    use quine_evolution::{CodeMutationError, CodeMutator};
    use quine_guardian::{AuditDecision, AuditLog};

    let dir = std::env::temp_dir().join(format!("quine-mut-it-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("custom_tool.rs");
    let original = "pub fn placeholder() -> u32 { 0 }\n";
    std::fs::write(&target, original).unwrap();
    let audit_path = dir.join("audit.log");

    // 1) Temiz mutasyon uygulanır.
    let clean = EchoBackend::fibonacci_solver();
    CodeMutator::new(&clean)
        .apply("echo", &target, "fibonacci ekle")
        .await
        .expect("temiz mutasyon uygulanmalı");
    assert!(std::fs::read_to_string(&target)
        .unwrap()
        .contains("fibonacci"));
    assert!(target.with_extension("rs.bak").exists(), "yedek alınmalı");

    // 2) Tehlikeli mutasyon engellenir; dosya korunur.
    let before = std::fs::read_to_string(&target).unwrap();
    let evil = EchoBackend::new(
        "```rust\nfn main() { std::process::Command::new(\"sh\").arg(\"-c\").arg(\"rm -rf /\").status().unwrap(); }\n```",
    );
    let err = CodeMutator::new(&evil)
        .apply("echo", &target, "kötü niyetli")
        .await
        .expect_err("guardian engellemeli");
    assert!(matches!(
        err.downcast_ref::<CodeMutationError>(),
        Some(CodeMutationError::Guardian(_))
    ));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);

    // 3) Engellenen eylem denetim kaydına yazılabilir (Faz 4 kabul kriteri).
    AuditLog::new(&audit_path)
        .record(AuditDecision::Blocked, None, &err.to_string())
        .unwrap();
    let logged = std::fs::read_to_string(&audit_path).unwrap();
    assert!(logged.contains("blocked"));
    assert!(logged.contains("SecurityViolation"));

    std::fs::remove_dir_all(&dir).ok();
}
