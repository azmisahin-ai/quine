//! Quine CLI — ajan framework'ünün kullanıcı arayüzü.
//!
//! Faz 0: `init`, `test-llm`
//! Faz 1: `run-once`
//! Faz 2: `evolve`
//! Faz 3: `population evolve`
//! Faz 4: `guard check`
//!
//! Simülasyon modu (`--simulate`) Ollama gerektirmeden tüm döngüyü
//! deterministik EchoBackend ile çalıştırır (CI ve ilk deneme için).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use quine_common::Agent;
use quine_eval::{Evaluator, Sandbox};
use quine_evolution::{
    evaluate_population, Archive, ArchiveEntry, MutationEngine, PopulationManager,
};
use quine_guardian::{AuditDecision, AuditLog, DiffAnalyzer};
use quine_llm::{LlmBackend, LlmRequest, OllamaBackend};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "quine",
    version,
    about = "Quine — kendi kendini yazan ajan framework'ü (%100 Rust, yerel LLM)"
)]
struct Cli {
    /// Ollama yerine deterministik simülasyon backend'i kullan (LLM'siz uçtan uca test).
    #[arg(long, global = true)]
    simulate: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// data/ dizinlerini ve varsayılan config'i hazırlar (Faz 0 / Adım 0.4)
    Init,
    /// Yerel LLM backend'ine ping atıp yanıtı yazdırır (Faz 0 kabul kriteri)
    TestLlm {
        /// Test istemi
        #[arg(long, default_value = "SADECE 'OK' kelimesini yanıtla.")]
        prompt: String,
    },
    /// Tek bir problemi uçtan uca çözer: LLM → kod → sandbox → puan (Faz 1)
    RunOnce {
        /// Problem id'si (fib-001 | rev-002 | sum-003)
        #[arg(long, default_value = "fib-001")]
        problem: String,
    },
    /// Öz-değişiklik döngüsü: başarısızsa prompt'u güncelle, tekrar dene (Faz 2)
    Evolve {
        #[arg(long, default_value_t = 5)]
        iterations: u32,
        #[arg(long, default_value = "fib-001")]
        problem: String,
    },
    /// Popülasyon komutları (Faz 3)
    #[command(subcommand)]
    Population(PopulationCommands),
    /// Guardian: dosya/diff güvenlik analizi (Faz 4)
    Guard {
        #[command(subcommand)]
        cmd: GuardCommands,
    },
}

#[derive(Subcommand)]
enum PopulationCommands {
    /// N jenerasyon paralel evrim çalıştırır ve arşivler
    Evolve {
        #[arg(long, default_value_t = 10)]
        generations: u32,
        #[arg(long, default_value_t = 6)]
        size: usize,
    },
    /// Arşivi yazdırır
    Show,
}

#[derive(Subcommand)]
enum GuardCommands {
    /// Bir dosyanın içeriğini tehlikeli pattern'lere karşı tarar
    Check {
        /// Taranacak dosya
        file: PathBuf,
    },
}

/// Backend seçimi: --simulate → EchoBackend, aksi halde Ollama.
fn backend_from_flags(simulate: bool) -> Arc<DynBackend> {
    if simulate {
        Arc::new(quine_llm::EchoBackend::fibonacci_solver())
    } else {
        Arc::new(OllamaBackend::from_env())
    }
}

type DynBackend = dyn LlmBackend + Send + Sync;

fn model_name(simulate: bool) -> String {
    if simulate {
        "simulate".into()
    } else {
        OllamaBackend::from_env().model().to_string()
    }
}

fn build_evaluator() -> Arc<Evaluator<Box<dyn Sandbox>>> {
    let sandbox = quine_eval::sandbox_from_env();
    tracing::info!("sandbox: {}", sandbox.kind());
    Arc::new(
        Evaluator::new(sandbox).with_audit(AuditLog::new(data_path("audit.log"))),
    )
}

fn data_path(name: &str) -> PathBuf {
    PathBuf::from("data").join(name)
}

fn select_problem(id: &str) -> Result<quine_common::Problem> {
    quine_bench_simple::SimpleBenchmark::problem(id)
        .with_context(|| format!("bilinmeyen problem '{id}'. Geçerliler: fib-001, rev-002, sum-003"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,quine_cli=info,quine_evolution=info,quine_eval=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init(),
        Commands::TestLlm { prompt } => cmd_test_llm(cli.simulate, prompt).await,
        Commands::RunOnce { problem } => cmd_run_once(cli.simulate, &problem).await,
        Commands::Evolve { iterations, problem } => {
            cmd_evolve(cli.simulate, iterations, &problem).await
        }
        Commands::Population(PopulationCommands::Evolve { generations, size }) => {
            cmd_population_evolve(cli.simulate, generations, size).await
        }
        Commands::Population(PopulationCommands::Show) => cmd_population_show(),
        Commands::Guard { cmd } => cmd_guard(cmd),
    }
}

// ---------------------------------------------------------------------------
// Faz 0
// ---------------------------------------------------------------------------

fn cmd_init() -> Result<()> {
    std::fs::create_dir_all("data")?;
    std::fs::create_dir_all("data/runs")?;
    let cfg = data_path("config.json");
    if !cfg.exists() {
        let default_cfg = serde_json::json!({
            "ollama_host": std::env::var("OLLAMA_HOST").unwrap_or_else(|_| quine_llm::DEFAULT_OLLAMA_HOST.into()),
            "model": std::env::var("QUINE_MODEL").unwrap_or_else(|_| quine_llm::DEFAULT_MODEL.into()),
            "sandbox": std::env::var("QUINE_SANDBOX").unwrap_or_else(|_| "local".into()),
        });
        std::fs::write(&cfg, serde_json::to_string_pretty(&default_cfg)?)?;
    }
    println!("✅ Quine init tamamlandı:");
    println!("   data/           → arşiv, log, sqlite dizini");
    println!("   data/config.json→ varsayılan yapılandırma");
    println!("Sonraki adım: docker compose up -d ollama && cargo run --bin quine -- test-llm");
    Ok(())
}

async fn cmd_test_llm(simulate: bool, prompt: String) -> Result<()> {
    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    println!(
        "🔌 {} backend'ine bağlanılıyor (model: {})…",
        backend.name(),
        model
    );

    // Önce sağlık kontrolü (Faz 0 kabul kriteri).
    backend
        .health_check()
        .await
        .context("LLM sağlık kontrolü başarısız — Ollama ayakta mı? Model kurulu mu?")?;

    let req = LlmRequest::new(model.clone(), "Kısa ve net yanıt ver.", prompt);
    let resp = backend.generate(&req).await.context("LLM üretim isteği")?;

    println!("📨 Yanıt ({}): {:?}", resp.model, resp.content.trim());
    println!("✅ LLM Bağlantısı Başarılı");
    Ok(())
}

// ---------------------------------------------------------------------------
// Faz 1
// ---------------------------------------------------------------------------

async fn cmd_run_once(simulate: bool, problem_id: &str) -> Result<()> {
    let problem = select_problem(problem_id)?;
    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    let evaluator = build_evaluator();

    let mut agent = Agent::new(format!("run-once-{}", problem.id));
    println!(
        "🤖 Ajan '{}' problem '{}' üzerinde çalışıyor (backend={}, model={})…",
        agent.name, problem.id, backend.name(), model
    );

    let req = LlmRequest::new(model.clone(), agent.system_prompt.clone(), problem.to_llm_prompt());
    let resp = backend.generate(&req).await.context("çözüm üretimi")?;
    let code = resp.extract_code();

    let result = evaluator.evaluate(agent.id, &problem, &code).await;
    agent.fitness_score = result.score;

    println!("🧾 Kod:\n{}\n", indent(&code));
    println!("📊 Sonuç: {}", serde_json::to_string_pretty(&result)?);
    if result.success {
        println!("✅ EvaluationResult {{ success: true, score: {:.1} }}", result.score);
    } else {
        println!("❌ Başarısız (score {:.1}, {}/{})", result.score, result.tests_passed, result.tests_total);
    }

    // Sonucu data/runs altına kalıcı yaz (ampirik doğrulama ilkesi).
    let run_file = data_path("runs").join(format!("run-{}.json", chrono_tag()));
    std::fs::create_dir_all(data_path("runs"))?;
    std::fs::write(&run_file, serde_json::to_string_pretty(&result)?)?;
    println!("💾 Kayıt: {}", run_file.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Faz 2
// ---------------------------------------------------------------------------

async fn cmd_evolve(simulate: bool, iterations: u32, problem_id: &str) -> Result<()> {
    let problem = select_problem(problem_id)?;
    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    let evaluator = build_evaluator();

    let mut agent = Agent::new(format!("evolve-{}", problem.id));
    let mut last_failures = Vec::new();

    for i in 1..=iterations {
        println!("— iterasyon {i}/{iterations} (jenerasyon {}, fitness {:.1}) —", agent.generation, agent.fitness_score);
        let req = LlmRequest::new(model.clone(), agent.system_prompt.clone(), problem.to_llm_prompt());
        let resp = backend.generate(&req).await.context("çözüm üretimi")?;
        let code = resp.extract_code();
        let result = evaluator.evaluate(agent.id, &problem, &code).await;
        agent.fitness_score = result.score;
        println!(
            "  → success={} score={:.1} ({}/{})",
            result.success, result.score, result.tests_passed, result.tests_total
        );

        if result.success {
            println!("✅ Problem çözüldü! Prompt güncellemeye gerek yok.");
            archive_agent(&agent);
            return Ok(());
        }

        last_failures.push(result);

        // Başarısızlık varsa: prompt'u evrimleştir (Adım 2.2).
        let engine = MutationEngine::new(backend_ref(&backend));
        match engine.refine_prompt(&agent, &model, &last_failures).await {
            Ok(new_prompt) => {
                println!("  🧬 Prompt güncellendi ({} karakter).", new_prompt.len());
                agent = agent.mutate_prompt(new_prompt);
            }
            Err(e) => {
                // Simülasyonda echo metni prompt olarak mantıklı olmayabilir; fallback mutasyon.
                tracing::warn!("refine başarısız ({e:#}), heuristik mutasyon uygulanıyor");
                agent = agent.mutate_prompt(format!(
                    "{}\n[Ders {}] Çıktıyı tam olarak beklenen formatta üret; kenar durumlarını (0, boş liste) kontrol et.",
                    agent.system_prompt, i
                ));
            }
        }
    }
    println!("⚠️ {} iterasyon sonunda problem hâlâ çözülmedi (fitness {:.1}).", iterations, agent.fitness_score);
    Ok(())
}

/// Arc<dyn> referansını generic LlmBackend olarak verir.
fn backend_ref(b: &Arc<dyn LlmBackend + Send + Sync>) -> &(dyn LlmBackend + Send + Sync) {
    b.as_ref()
}

fn archive_agent(agent: &Agent) {
    let archive = Archive::default_location();
    let entry = ArchiveEntry {
        agent: agent.clone(),
        best_score: agent.fitness_score,
        generations_survived: agent.generation,
    };
    if let Err(e) = archive.upsert(entry, 50) {
        tracing::error!("arşive yazılamadı: {e:#}");
    } else {
        println!("🗄️ Ajan arşivlendi: data/archive.json");
    }
}

// ---------------------------------------------------------------------------
// Faz 3
// ---------------------------------------------------------------------------

async fn cmd_population_evolve(simulate: bool, generations: u32, size: usize) -> Result<()> {
    use quine_bench_simple::SimpleBenchmark;

    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    let evaluator = build_evaluator();
    let pm = PopulationManager::new(size, 2);
    let problems = SimpleBenchmark::problems();

    let mut population: Vec<Agent> = (0..size).map(|i| Agent::new(format!("root-{i}"))).collect();

    for gen in 1..=generations {
        // Paralel değerlendirme (Adım 3.3: tokio::spawn).
        let scored = evaluate_population(
            backend.clone_any(),
            evaluator.clone(),
            population.clone(),
            problems.clone(),
            model.clone(),
        )
        .await;

        let mut agents: Vec<Agent> = scored.iter().map(|(a, _)| a.clone()).collect();
        let avg = if agents.is_empty() {
            0.0
        } else {
            agents.iter().map(|a| a.fitness_score).sum::<f64>() / agents.len() as f64
        };
        let best = agents.iter().map(|a| a.fitness_score).fold(0.0f64, f64::max);
        println!("jenerasyon {gen}: ortalama fitness {avg:.1}, en iyi {best:.1}");

        // Başarılı olanları arşivle (Adım 3.1).
        for (agent, results) in &scored {
            if results.iter().any(|r| r.success) {
                archive_agent(agent);
            }
        }

        // Seçilim + çocuk üretimi (Adım 3.2).
        let parents = pm.select_parents_tournament(&agents, 3, size);
        agents.sort_by(|a, b| b.fitness_score.partial_cmp(&a.fitness_score).unwrap());
        population = pm.next_generation(&agents, &parents);
    }
    println!("✅ Popülasyon evrimi bitti. Arşiv: data/archive.json");
    Ok(())
}

/// Arc<dyn LlmBackend>'i generic spawn'a uygun şekilde çoğaltmak için yardımcı trait.
trait CloneAny {
    fn clone_any(&self) -> Arc<DynBackend>;
}

impl CloneAny for Arc<DynBackend> {
    fn clone_any(&self) -> Arc<DynBackend> {
        self.clone()
    }
}

fn cmd_population_show() -> Result<()> {
    let entries = Archive::default_location().load()?;
    if entries.is_empty() {
        println!("Arşiv boş (data/archive.json). Önce `quine population evolve` çalıştırın.");
        return Ok(());
    }
    println!("{:<38} {:<16} {:>4} {:>8}", "AGENT ID", "NAME", "GEN", "FITNESS");
    for e in &entries {
        println!(
            "{:<38} {:<16} {:>4} {:>8.1}",
            e.agent.id, e.agent.name, e.agent.generation, e.best_score
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Faz 4
// ---------------------------------------------------------------------------

fn cmd_guard(cmd: GuardCommands) -> Result<()> {
    let GuardCommands::Check { file } = cmd;
    let content = std::fs::read_to_string(&file)
        .with_context(|| format!("dosya okunamadı: {}", file.display()))?;
    let analyzer = DiffAnalyzer::default();
    let audit = AuditLog::default_location();
    match analyzer.analyze(&content) {
        Ok(()) => {
            audit.record(AuditDecision::Allowed, None, &format!("guard check {}", file.display()))?;
            println!("✅ Guardian: '{}' temiz.", file.display());
            for f in analyzer.scan(&content) {
                println!("   ⚠ uyarı [{}] satır {}: {}", f.rule, f.line_number, f.excerpt);
            }
            Ok(())
        }
        Err(violation) => {
            audit.record(AuditDecision::Blocked, None, &violation.to_string())?;
            println!("🚫 SecurityViolation — '{}':", file.display());
            for v in &violation.violations {
                println!("   ✗ [{}] satır {}: {}", v.rule, v.line_number, v.excerpt);
            }
            println!("   (kayıt: data/audit.log)");
            anyhow::bail!("{violation}");
        }
    }
}

fn chrono_tag() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}
