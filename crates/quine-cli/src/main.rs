//! Quine CLI — ajan framework'ünün kullanıcı arayüzü.
//!
//! Faz 0: `init`, `test-llm`
//! Faz 1: `run-once`
//! Faz 2: `evolve`
//! Faz 3: `population evolve`
//! Faz 4: `guard check`, `mutate`
//!
//! Simülasyon modu (`--simulate`) Ollama gerektirmeden tüm döngüyü
//! deterministik EchoBackend ile çalıştırır (CI ve ilk deneme için).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use quine_common::{Agent, EvaluationResult};
use quine_eval::Evaluator;
use quine_evolution::{
    evaluate_population, evolve_step, Archive, ArchiveEntry, CodeMutationError, CodeMutator,
    PopulationManager,
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
        /// Yerleşik problem id'si (fib-001 | rev-002 | sum-003)
        #[arg(long, default_value = "fib-001")]
        problem: String,
        /// Dışarıdan problem tanımı (JSON). Verilirse `--problem` yok sayılır.
        #[arg(long, value_name = "FILE")]
        problem_file: Option<PathBuf>,
    },
    /// Öz-değişiklik döngüsü: başarısızsa prompt'u güncelle, tekrar dene (Faz 2)
    Evolve {
        #[arg(long, default_value_t = 5)]
        iterations: u32,
        #[arg(long, default_value = "fib-001")]
        problem: String,
        /// Dışarıdan problem tanımı (JSON). Verilirse `--problem` yok sayılır.
        #[arg(long, value_name = "FILE")]
        problem_file: Option<PathBuf>,
    },
    /// Popülasyon komutları (Faz 3)
    #[command(subcommand)]
    Population(PopulationCommands),
    /// Guardian: dosya/diff güvenlik analizi (Faz 4)
    Guard {
        #[command(subcommand)]
        cmd: GuardCommands,
    },
    /// Gerçek kaynak-kod mutasyonu: LLM önerisi guardian'dan geçerse uygulanır (Faz 4)
    Mutate {
        /// Mutasyona uğratılacak kaynak dosya
        file: PathBuf,
        /// LLM'e verilecek değişiklik talimatı
        #[arg(long)]
        instruction: String,
        /// Kuru çalıştırma: üret ve tara, ama diske yazma
        #[arg(long)]
        dry_run: bool,
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
        let cfg = load_config();
        Arc::new(OllamaBackend::new(
            resolved_host(&cfg),
            resolved_model(&cfg),
        ))
    }
}

type DynBackend = dyn LlmBackend + Send + Sync;

fn model_name(simulate: bool) -> String {
    if simulate {
        "simulate".into()
    } else {
        resolved_model(&load_config())
    }
}

fn build_evaluator() -> Result<Arc<Evaluator>> {
    let cfg = load_config();
    let sandbox = quine_eval::sandbox_from_kind(&resolved_sandbox(&cfg))?;
    tracing::info!("sandbox: {}", sandbox.kind());
    if sandbox.kind() == "local" {
        eprintln!(
            "⚠️  Yerel sandbox etkin: LLM kodu bu makinede doğrudan çalıştırılıyor. \
             Gerçek kullanımda `QUINE_SANDBOX=docker` (veya data/config.json → \
             \"sandbox\": \"docker\") tercih edin."
        );
    }
    Ok(Arc::new(
        Evaluator::new(sandbox).with_audit(AuditLog::new(data_path("audit.log"))),
    ))
}

/// `data/config.json` içeriği (tüm alanlar isteğe bağlı).
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    ollama_host: Option<String>,
    model: Option<String>,
    sandbox: Option<String>,
}

/// `data/config.json`'ı okur; dosya yoksa/bozuksa varsayılanlara düşer.
fn load_config() -> FileConfig {
    let path = data_path("config.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!("config.json geçersiz ({e}); varsayılanlar kullanılıyor");
            FileConfig::default()
        }),
        Err(_) => FileConfig::default(),
    }
}

/// Öncelik sırası: ortam değişkeni > config.json > yerleşik varsayılan.
fn resolved_host(cfg: &FileConfig) -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .or_else(|| cfg.ollama_host.clone())
        .unwrap_or_else(|| quine_llm::DEFAULT_OLLAMA_HOST.into())
}

fn resolved_model(cfg: &FileConfig) -> String {
    std::env::var("QUINE_MODEL")
        .ok()
        .or_else(|| cfg.model.clone())
        .unwrap_or_else(|| quine_llm::DEFAULT_MODEL.into())
}

fn resolved_sandbox(cfg: &FileConfig) -> String {
    std::env::var("QUINE_SANDBOX")
        .ok()
        .or_else(|| cfg.sandbox.clone())
        .unwrap_or_else(|| "local".into())
}

fn data_path(name: &str) -> PathBuf {
    PathBuf::from("data").join(name)
}

fn select_problem(id: &str) -> Result<quine_common::Problem> {
    quine_bench_simple::SimpleBenchmark::problem(id).with_context(|| {
        format!("bilinmeyen problem '{id}'. Geçerliler: fib-001, rev-002, sum-003")
    })
}

/// Problem tanımını yerleşik setten veya dış JSON dosyasından yükler.
///
/// `--problem-file` verilirse dosya okunur ve `--problem` yok sayılır; aksi
/// halde yerleşik id kullanılır.
fn load_problem(id: &str, file: Option<&std::path::Path>) -> Result<quine_common::Problem> {
    match file {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("problem dosyası okunamadı: {}", path.display()))?;
            let problem: quine_common::Problem = serde_json::from_str(&raw)
                .with_context(|| format!("problem dosyası geçersiz JSON: {}", path.display()))?;
            validate_problem(&problem)?;
            println!(
                "📥 Dış problem yüklendi: {} ({})",
                problem.id,
                path.display()
            );
            Ok(problem)
        }
        None => select_problem(id),
    }
}

/// Dış problem tanımının değerlendirilebilir olduğunu doğrular.
fn validate_problem(p: &quine_common::Problem) -> Result<()> {
    if p.id.trim().is_empty() {
        anyhow::bail!("problem 'id' boş olamaz");
    }
    if p.function_signature.trim().is_empty() {
        anyhow::bail!("problem 'function_signature' boş olamaz");
    }
    if p.test_cases.is_empty() {
        anyhow::bail!("problem 'test_cases' en az bir girdi içermeli");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "warn,quine_cli=info,quine_evolution=info,quine_eval=info".into()
            }),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init(),
        Commands::TestLlm { prompt } => cmd_test_llm(cli.simulate, prompt).await,
        Commands::RunOnce {
            problem,
            problem_file,
        } => cmd_run_once(cli.simulate, &problem, problem_file.as_deref()).await,
        Commands::Evolve {
            iterations,
            problem,
            problem_file,
        } => cmd_evolve(cli.simulate, iterations, &problem, problem_file.as_deref()).await,
        Commands::Population(PopulationCommands::Evolve { generations, size }) => {
            cmd_population_evolve(cli.simulate, generations, size).await
        }
        Commands::Population(PopulationCommands::Show) => cmd_population_show(),
        Commands::Guard { cmd } => cmd_guard(cmd),
        Commands::Mutate {
            file,
            instruction,
            dry_run,
        } => cmd_mutate(cli.simulate, &file, &instruction, dry_run).await,
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

async fn cmd_run_once(
    simulate: bool,
    problem_id: &str,
    problem_file: Option<&std::path::Path>,
) -> Result<()> {
    let problem = load_problem(problem_id, problem_file)?;
    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    let evaluator = build_evaluator()?;

    let mut agent = Agent::new(format!("run-once-{}", problem.id));
    println!(
        "🤖 Ajan '{}' problem '{}' üzerinde çalışıyor (backend={}, model={})…",
        agent.name,
        problem.id,
        backend.name(),
        model
    );

    let req = LlmRequest::new(
        model.clone(),
        agent.system_prompt.clone(),
        problem.to_llm_prompt(),
    );
    let resp = backend.generate(&req).await.context("çözüm üretimi")?;
    let code = resp.extract_code();

    let result = evaluator.evaluate(agent.id, &problem, &code).await;
    agent.fitness_score = result.score;

    println!("🧾 Kod:\n{}\n", indent(&code));
    println!("📊 Sonuç: {}", serde_json::to_string_pretty(&result)?);
    if result.success {
        println!(
            "✅ EvaluationResult {{ success: true, score: {:.1} }}",
            result.score
        );
    } else {
        println!(
            "❌ Başarısız (score {:.1}, {}/{})",
            result.score, result.tests_passed, result.tests_total
        );
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

async fn cmd_evolve(
    simulate: bool,
    iterations: u32,
    problem_id: &str,
    problem_file: Option<&std::path::Path>,
) -> Result<()> {
    let problem = load_problem(problem_id, problem_file)?;
    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    let evaluator = build_evaluator()?;

    let mut agent = Agent::new(format!("evolve-{}", problem.id));
    let mut failures: Vec<EvaluationResult> = Vec::new();
    let mut best = agent.fitness_score;

    for i in 1..=iterations {
        println!(
            "— iterasyon {i}/{iterations} (jenerasyon {}, fitness {:.1}) —",
            agent.generation, agent.fitness_score
        );
        let step = evolve_step(
            backend.as_ref(),
            &evaluator,
            &agent,
            &problem,
            &model,
            &failures,
        )
        .await?;
        println!("  🧾 Üretilen kod:\n{}", indent(&step.code));
        println!(
            "  → success={} score={:.1} ({}/{})",
            step.result.success,
            step.result.score,
            step.result.tests_passed,
            step.result.tests_total
        );
        if !step.result.success && !step.result.stderr.trim().is_empty() {
            let err = step
                .result
                .stderr
                .lines()
                .take(4)
                .collect::<Vec<_>>()
                .join("\n");
            println!("  ⚠️ hata: {err}");
        }

        if step.result.success {
            println!("✅ Problem çözüldü (iterasyon {i}).");
            archive_agent(&step.agent);
            return Ok(());
        }

        if step.accepted {
            println!(
                "  🧬 Mutasyon kabul edildi (fitness {:.1} → {:.1}).",
                best, step.result.score
            );
            if std::env::var("QUINE_SHOW_PROMPT").is_ok() {
                println!("  📝 Yeni prompt:\n{}", indent(&step.agent.system_prompt));
            }
        } else {
            println!("  ↩️ Mutasyon reddedildi (fitness {:.1} korunuyor).", best);
        }
        best = step.agent.fitness_score;
        failures.push(step.result);
        agent = step.agent;
    }
    println!(
        "⚠️ {iterations} iterasyon sonunda problem hâlâ çözülmedi (en iyi fitness {:.1}).",
        best
    );
    archive_agent(&agent);
    Ok(())
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
    let evaluator = build_evaluator()?;
    let pm = PopulationManager::new(size, 2);
    let problems = SimpleBenchmark::problems();

    let mut population: Vec<Agent> = (0..size).map(|i| Agent::new(format!("root-{i}"))).collect();

    for gen in 1..=generations {
        // Paralel değerlendirme (Adım 3.3: tokio::spawn).
        let scored = evaluate_population(
            backend.clone(),
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
        let best = agents
            .iter()
            .map(|a| a.fitness_score)
            .fold(0.0f64, f64::max);
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

fn cmd_population_show() -> Result<()> {
    let entries = Archive::default_location().load()?;
    if entries.is_empty() {
        println!("Arşiv boş (data/archive.json). Önce `quine population evolve` çalıştırın.");
        return Ok(());
    }
    println!(
        "{:<38} {:<16} {:>4} {:>8}",
        "AGENT ID", "NAME", "GEN", "FITNESS"
    );
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

async fn cmd_mutate(
    simulate: bool,
    file: &std::path::Path,
    instruction: &str,
    dry_run: bool,
) -> Result<()> {
    if !file.exists() {
        anyhow::bail!("hedef dosya yok: {}", file.display());
    }
    let backend = backend_from_flags(simulate);
    let model = model_name(simulate);
    let mutator = CodeMutator::new(backend.as_ref());

    if dry_run {
        // Öneriyi üret ve guardian taramasından geçir; diske dokunma.
        let code = mutator.propose(&model, file, instruction).await?;
        match DiffAnalyzer::default().analyze(&code) {
            Ok(()) => {
                println!("✅ Öneri guardian'dan geçti (kuru çalıştırma, yazılmadı).");
                println!("{}", indent(&code));
            }
            Err(v) => {
                println!("🚫 Öneri engellendi: {v}");
                AuditLog::default_location().record(
                    AuditDecision::Blocked,
                    None,
                    &format!("mutate --dry-run {}: {v}", file.display()),
                )?;
                return Err(v.into());
            }
        }
        return Ok(());
    }

    match mutator.apply(&model, file, instruction).await {
        Ok(path) => {
            AuditLog::default_location().record(
                AuditDecision::Allowed,
                None,
                &format!("mutate {}: {}", path.display(), instruction),
            )?;
            println!("✅ Mutasyon uygulandı: {}", path.display());
            Ok(())
        }
        Err(e) => {
            let detail = format!("mutate {}: {e}", file.display());
            if matches!(
                e.downcast_ref::<CodeMutationError>(),
                Some(CodeMutationError::Guardian(_))
            ) {
                AuditLog::default_location().record(AuditDecision::Blocked, None, &detail)?;
            }
            Err(e)
        }
    }
}

fn cmd_guard(cmd: GuardCommands) -> Result<()> {
    let GuardCommands::Check { file } = cmd;
    let content = std::fs::read_to_string(&file)
        .with_context(|| format!("dosya okunamadı: {}", file.display()))?;
    let analyzer = DiffAnalyzer::default();
    let audit = AuditLog::default_location();
    match analyzer.analyze(&content) {
        Ok(()) => {
            audit.record(
                AuditDecision::Allowed,
                None,
                &format!("guard check {}", file.display()),
            )?;
            println!("✅ Guardian: '{}' temiz.", file.display());
            for f in analyzer.scan(&content) {
                println!(
                    "   ⚠ uyarı [{}] satır {}: {}",
                    f.rule, f.line_number, f.excerpt
                );
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
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
