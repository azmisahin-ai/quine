//! # quine-web
//!
//! Quine'ın **yerel web kontrol düzlemi**: axum tabanlı HTTP API + canlı SSE
//! akışı + tek dosyalık gömülü (vanilla JS) dashboard.
//!
//! Tasarım kararları:
//! * **Yalnızca yerel**: sunucu varsayılan olarak `127.0.0.1`'e bağlanır.
//! * **Fail-closed sandbox**: geçersiz sandbox türü istek reddedilir.
//! * **Kaynak koruması**: eşzamanlı run limiti (doluysa `429`), gövde boyutu
//!   limiti, model/alan doğrulaması.
//! * **Sızıntı yok**: yanıtlar yapılandırılmış JSON; iç yığın izi istemciye
//!   gönderilmez.
//! * **Güvenlik başlıkları**: CSP, `nosniff`, `DENY` frame, referrer policy.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::{get, post},
    Router,
};
use futures::StreamExt;
use quine_bench_simple::SimpleBenchmark;
use quine_common::Problem;
use quine_llm::{LlmBackend, ScriptedBackend};
use quine_runtime::{
    RunConfig, RunControl, RunEngine, RunEvent, RunMode, RunRequest, RuntimeContext,
};
use quine_storage::WorkloadKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, Semaphore};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;

/// Gövde boyutu üst sınırı (JSON problem tanımı için fazlasıyla yeterli).
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Tek bir istekte kabul edilen en uzun model adı.
const MAX_MODEL_LEN: usize = 128;
/// Eşzamanlı run limiti (kaynak tükenmesini engeller).
const MAX_CONCURRENT_RUNS: usize = 4;

/// Uygulama durumu (paylaşılan).
#[derive(Clone)]
pub struct AppState {
    pub ctx: RuntimeContext,
    pub engine: Arc<RunEngine>,
    pub controls: Arc<Mutex<HashMap<String, RunControl>>>,
    slots: Arc<Semaphore>,
    defaults: RunConfig,
    simulate: bool,
}

impl AppState {
    pub fn new(ctx: RuntimeContext, defaults: RunConfig) -> Self {
        let engine = Arc::new(RunEngine::new(ctx.clone()));
        Self {
            ctx,
            engine,
            controls: Arc::new(Mutex::new(HashMap::new())),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS)),
            defaults,
            simulate: false,
        }
    }

    /// Demo (ağsız) modda çalıştırır.
    pub fn with_simulate(mut self, on: bool) -> Self {
        self.simulate = on;
        self
    }
}

/// HTTP API hatası → yapılandırılmış JSON (iç detay sızdırmaz).
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

/// Tüm rotaları ve ara katmanları (middleware) kuran yönlendirici.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/metrics", get(metrics))
        .route("/api/audit", get(audit))
        .route("/api/problems", get(problems))
        .route("/api/runs", get(list_runs).post(start_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/stream", get(run_stream))
        .route("/api/runs/{id}/candidates", get(run_candidates))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/pause", post(pause_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            header::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            header::HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            header::HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(
                "default-src 'none'; script-src 'self'; style-src 'self'; \
                 connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
                 form-action 'none'; frame-ancestors 'none'",
            ),
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Statik varlıklar
// ---------------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response()
}

async fn js() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Sistem durumu
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: String,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: quine_runtime::VERSION.to_string(),
    })
}

#[derive(Serialize)]
struct SystemStatus {
    version: String,
    ollama_host: String,
    ollama_reachable: bool,
    ollama_models: Vec<String>,
    docker_available: bool,
    sandbox_default: String,
    storage_path: String,
    simulate: bool,
}

async fn status(State(app): State<AppState>) -> Json<SystemStatus> {
    let host =
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| quine_llm::DEFAULT_OLLAMA_HOST.into());
    let backend = quine_llm::OllamaBackend::new(host.clone(), app.defaults.model.clone());
    let (reachable, models) = match backend.list_models().await {
        Ok(m) => (true, m),
        Err(_) => (false, Vec::new()),
    };
    Json(SystemStatus {
        version: quine_runtime::VERSION.to_string(),
        ollama_host: host,
        ollama_reachable: reachable,
        ollama_models: models,
        docker_available: quine_eval::DockerSandbox::available(),
        sandbox_default: app.defaults.sandbox_kind.clone(),
        storage_path: app.ctx.store.path().display().to_string(),
        simulate: app.simulate,
    })
}

async fn metrics(State(app): State<AppState>) -> Result<Json<quine_storage::Metrics>, ApiError> {
    app.ctx
        .store
        .metrics()
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    100
}

async fn audit(
    State(app): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<quine_storage::AuditEntry>>, ApiError> {
    let limit = q.limit.min(1000);
    app.ctx
        .store
        .list_audit(limit)
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Serialize)]
struct ProblemInfo {
    id: String,
    title: String,
    description: String,
}

async fn problems() -> Json<Vec<ProblemInfo>> {
    Json(
        SimpleBenchmark::problems()
            .into_iter()
            .map(|p| ProblemInfo {
                id: p.id,
                title: p.title,
                description: p.description,
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Run listesi / detayı
// ---------------------------------------------------------------------------

async fn list_runs(
    State(app): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<quine_storage::RunRecord>>, ApiError> {
    app.ctx
        .store
        .list_runs(q.limit.min(1000))
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn get_run(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<quine_storage::RunRecord>, ApiError> {
    app.ctx
        .store
        .get_run(&id)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .ok_or_else(|| not_found(format!("run bulunamadı: {id}")))
}

#[derive(Deserialize)]
struct AfterQuery {
    #[serde(default)]
    after: i64,
}

async fn run_events(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AfterQuery>,
) -> Result<Json<Vec<RunEvent>>, ApiError> {
    app.ctx
        .bus
        .history(&id, q.after)
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn run_candidates(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<quine_storage::CandidateRecord>>, ApiError> {
    app.ctx
        .store
        .list_candidates(&id)
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---------------------------------------------------------------------------
// Run başlatma (doğrulama + limit)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StartRunBody {
    problem_id: Option<String>,
    problem: Option<Problem>,
    mode: Option<String>,
    model: Option<String>,
    sandbox: Option<String>,
    simulate: Option<bool>,
    population_size: Option<usize>,
    temperature: Option<f64>,
}

#[derive(Serialize)]
struct StartRunResponse {
    run_id: String,
}

fn parse_mode(s: &str) -> Option<RunMode> {
    Some(match s {
        "single" => RunMode::Single,
        "evolve" => RunMode::Evolve,
        "population" => RunMode::Population,
        _ => return None,
    })
}

/// Model adını doğrular: boş değil, makul uzunlukta, kontrol karakteri yok.
fn validate_model(model: &str) -> Result<(), ApiError> {
    let m = model.trim();
    if m.is_empty() {
        return Err(bad_request("model boş olamaz"));
    }
    if m.len() > MAX_MODEL_LEN {
        return Err(bad_request("model adı çok uzun"));
    }
    if m.chars().any(|c| c.is_control()) {
        return Err(bad_request("model adı kontrol karakteri içeremez"));
    }
    Ok(())
}

async fn start_run(
    State(app): State<AppState>,
    Json(body): Json<StartRunBody>,
) -> Result<Json<StartRunResponse>, ApiError> {
    // Problem seçimi: id VEYA tam tanım (ikisi birden verilemez).
    let problem = match (&body.problem_id, &body.problem) {
        (Some(_), Some(_)) => return Err(bad_request("problem_id ve problem birlikte verilemez")),
        (Some(id), None) => SimpleBenchmark::problem(id)
            .ok_or_else(|| bad_request(format!("bilinmeyen problem: {id}")))?,
        (None, Some(p)) => {
            if p.test_cases.is_empty() {
                return Err(bad_request("problem test_cases boş olamaz"));
            }
            p.clone()
        }
        (None, None) => return Err(bad_request("problem_id veya problem gerekli")),
    };

    // Sandbox: fail-closed (geçersiz tür reddedilir; docker yoksa hata).
    // Demo modunda docker gerekmemesi kullanıcı için kritik bir kolaylıktır.
    let simulate = body.simulate.unwrap_or(app.simulate);
    let mut sandbox_kind = body
        .sandbox
        .clone()
        .unwrap_or_else(|| app.defaults.sandbox_kind.clone());
    if simulate && body.sandbox.is_none() {
        sandbox_kind = "local".into();
    }
    let resolved = quine_eval::sandbox_from_kind(&sandbox_kind)
        .map_err(|e| bad_request(format!("sandbox: {e}")))?;
    let sandbox_kind = resolved.kind().to_string();

    let mode = match body.mode.as_deref() {
        None => app.defaults.mode,
        Some(s) => {
            parse_mode(s).ok_or_else(|| bad_request("geçersiz mode (single|evolve|population)"))?
        }
    };

    let model = body
        .model
        .clone()
        .unwrap_or_else(|| app.defaults.model.clone());
    validate_model(&model)?;

    let mut config = app.defaults.clone();
    config.model = model.clone();
    config.mode = mode;
    config.sandbox_kind = sandbox_kind.clone();
    config.population_size = body
        .population_size
        .unwrap_or(config.population_size)
        .clamp(1, config.limits.max_population);
    if let Some(t) = body.temperature {
        if !(0.0..=2.0).contains(&t) {
            return Err(bad_request("temperature 0.0..=2.0 aralığında olmalı"));
        }
        config.temperature = t;
    }

    // Eşzamanlılık limiti: kapasite yoksa hemen 429 (kuyruğa alıp sessizce
    // beklemiyoruz — istemci ne olduğunu bilmeli).
    let permit = app.slots.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            format!("eşzamanlı run limiti ({MAX_CONCURRENT_RUNS}) dolu"),
        )
    })?;

    let backend: Option<Arc<dyn LlmBackend>> = if simulate {
        // Demo: gözle görülür adımlar + öğrenme döngüsü (yanlış kod → düzeltme).
        // Yerel sandbox kullanılır çünkü demo hiçbir ağ erişimi gerektirmez ve
        // Docker'a bağımlı olursa "LLM'siz de çalışır" sözü tutulmaz olur.
        Some(Arc::new(
            ScriptedBackend::fail_then_fix(
                "```rust\npub fn fibonacci(n: u32) -> u64 { let x: u64 = \"wrong\"; x }\n```",
                "```rust\npub fn fibonacci(n: u32) -> u64 {\n    let (mut a, mut b) = (0u64, 1u64);\n    for _ in 0..n { let t = a + b; a = b; b = t; }\n    a\n}\n```",
            )
            .with_delay(Duration::from_millis(900)),
        ))
    } else {
        None
    };

    let req = RunRequest {
        problem,
        workload: if simulate {
            WorkloadKind::Demo
        } else {
            WorkloadKind::Production
        },
        config,
        backend,
    };

    let handle = app
        .engine
        .start(req)
        .map_err(|e| bad_request(format!("run başlatılamadı: {e}")))?;
    let run_id = handle.run_id.clone();
    let control = handle.control();
    app.controls.lock().unwrap().insert(run_id.clone(), control);

    // Run bitince kontrolü ve slot'u serbest bırak.
    let controls = app.controls.clone();
    let id = run_id.clone();
    tokio::spawn(async move {
        let _outcome = handle.wait().await;
        controls.lock().unwrap().remove(&id);
        drop(permit);
    });

    Ok(Json(StartRunResponse { run_id }))
}

// ---------------------------------------------------------------------------
// Kumanda (cancel / pause / resume)
// ---------------------------------------------------------------------------

fn with_control<F>(app: &AppState, id: &str, f: F) -> Result<Json<serde_json::Value>, ApiError>
where
    F: FnOnce(&RunControl),
{
    let controls = app.controls.lock().unwrap();
    match controls.get(id) {
        Some(c) => {
            f(c);
            Ok(Json(serde_json::json!({ "ok": true, "run_id": id })))
        }
        None => Err(not_found(format!(
            "aktif run bulunamadı: {id} (bitmiş veya yok)"
        ))),
    }
}

async fn cancel_run(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_control(&app, &id, |c| c.cancel())
}

async fn pause_run(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_control(&app, &id, |c| c.pause())
}

async fn resume_run(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_control(&app, &id, |c| c.resume())
}

// ---------------------------------------------------------------------------
// SSE canlı akış
// ---------------------------------------------------------------------------

async fn run_stream(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = app.ctx.bus.subscribe();
    let history = app.ctx.bus.history(&id, 0).unwrap_or_default();
    let last = history.last().map(|e| e.sequence).unwrap_or(0);

    // Geçmişi gönder, sonra canlı akışa geç; sıra numarasına göre tekilleştir.
    let hist = futures::stream::iter(
        history
            .into_iter()
            .map(|e| Ok::<_, std::convert::Infallible>(to_sse(&e))),
    );

    let run_id = id.clone();
    let live = futures::stream::unfold(
        (rx, last, run_id),
        |(mut rx, mut last, run_id)| async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev.run_id != run_id || ev.sequence <= last {
                            continue;
                        }
                        last = ev.sequence;
                        return Some((Ok(to_sse(&ev)), (rx, last, run_id)));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(hist.chain(live)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn to_sse(e: &RunEvent) -> Event {
    Event::default()
        .event(e.kind.as_str())
        .id(e.sequence.to_string())
        .json_data(e)
        .unwrap_or_else(|_| Event::default().event("ERROR"))
}

// ---------------------------------------------------------------------------
// Gömülü UI (vanilla JS; harici CDN yok)
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_CSS: &str = include_str!("../static/app.css");
const APP_JS: &str = include_str!("../static/app.js");

/// Sunucuyu verilen adreste çalıştırır (bloklar).
pub async fn serve(addr: std::net::SocketAddr, state: AppState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("adres dinlenemedi ({addr}): {e}"))?;
    tracing::info!("Quine web kontrol düzlemi: http://{addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use quine_storage::Store;
    use tower::util::ServiceExt; // oneshot

    fn state() -> AppState {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let ctx = RuntimeContext::new(store, std::env::temp_dir());
        let cfg = RunConfig {
            sandbox_kind: "local".into(),
            ..RunConfig::default()
        };
        AppState::new(ctx, cfg).with_simulate(true)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn health_and_status_ok() {
        let app = router(state());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert!(j.get("version").is_some());
    }

    #[tokio::test]
    async fn security_headers_are_present() {
        let app = router(state());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert!(resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
    }

    #[tokio::test]
    async fn start_run_rejects_invalid_sandbox() {
        let app = router(state());
        let body = serde_json::json!({"problem_id": "fib-001", "sandbox": "banana"});
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn start_run_rejects_bad_inputs() {
        let app = router(state());
        let full_problem = serde_json::json!({
            "id": "x", "title": "t", "description": "d",
            "function_signature": "pub fn f() -> i64 { todo!() }",
            "test_cases": [{"input": 1, "expected": 1}]
        });
        for body in [
            serde_json::json!({"problem_id": "fib-001", "mode": "nope"}),
            serde_json::json!({"problem_id": "fib-001", "model": ""}),
            serde_json::json!({"problem_id": "fib-001", "model": "a".repeat(500)}),
            serde_json::json!({"problem_id": "fib-001", "temperature": 9.0}),
            serde_json::json!({}),
            serde_json::json!({"problem_id": "fib-001", "problem": full_problem}),
            serde_json::json!({"problem_id": "unknown-problem"}),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/runs")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "body={body}");
        }
    }

    #[tokio::test]
    async fn start_simulated_run_completes_via_api() {
        let st = state();
        let store = st.ctx.store.clone();
        let app = router(st);
        let body = serde_json::json!({"problem_id": "fib-001", "mode": "evolve", "simulate": true});
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        let id = j["run_id"].as_str().unwrap().to_string();

        // Run bitene kadar bekle.
        for _ in 0..400 {
            if let Ok(Some(r)) = store.get_run(&id) {
                if r.status.is_terminal() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let r = store.get_run(&id).unwrap().unwrap();
        assert!(r.status.is_terminal(), "run tamamlanmalı: {:?}", r.status);
        assert!(r.production_success);

        // Olaylar ve adaylar API'den okunabilir.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/runs/{id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evs = body_json(resp).await;
        assert!(evs.as_array().unwrap().len() > 3);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/runs/{id}/candidates"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_run_returns_404() {
        let app = router(state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/runs/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_unknown_run_is_404() {
        let app = router(state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs/nope/cancel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let app = router(state());
        let big = "x".repeat(MAX_BODY_BYTES + 1024);
        let body = serde_json::json!({"problem_id": "fib-001", "model": big});
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status() == StatusCode::PAYLOAD_TOO_LARGE
                || resp.status() == StatusCode::BAD_REQUEST,
            "got {}",
            resp.status()
        );
    }
}
