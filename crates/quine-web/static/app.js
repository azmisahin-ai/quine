"use strict";

// Quine dashboard — bağımlılıksız vanilla JS. Tüm veriler yerel API'den gelir.
const $ = (id) => document.getElementById(id);
const api = {
  async get(path) {
    const r = await fetch(path, { headers: { Accept: "application/json" } });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.statusText);
    return r.json();
  },
  async post(path, body) {
    const r = await fetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.statusText);
    return r.json();
  },
};

let currentRun = null;
let es = null;
let pollTimer = null;

// ---- İnsan diline çeviri -------------------------------------------------
const KIND = {
  RUN_STARTED:        ["Çalışma başladı", "Ajan görevi devraldı", "warn"],
  PROBLEM_LOADED:     ["Problem yüklendi", null, "warn"],
  LLM_REQUEST_STARTED:["Modelden kod istendi", "LLM'e istek gönderildi", "warn"],
  LLM_RESPONSE_RECEIVED:["Model yanıt verdi", null, "warn"],
  CODE_EXTRACTED:     ["Kod ayıklandı", "Modelin yanıtından çalıştırılabilir kod çıkarıldı", "warn"],
  GUARDIAN_ALLOWED:   ["Güvenlik kontrolü: geçti", "Tehlikeli desen bulunmadı", "good"],
  GUARDIAN_BLOCKED:   ["Güvenlik kontrolü: ENGELLENDİ", "Tehlikeli kod sandbox'a gönderilmedi", "bad"],
  SANDBOX_STARTED:    ["İzole ortamda derleniyor", null, "warn"],
  SANDBOX_COMPLETED:  ["Derleme/çalıştırma bitti", null, "warn"],
  COMPILATION_FAILED: ["Derleme hatası", null, "bad"],
  TESTS_STARTED:      ["Testler çalışıyor", null, "warn"],
  TESTS_COMPLETED:    ["Testler bitti", null, "warn"],
  EVALUATION_COMPLETED:["Değerlendirme tamam", null, "warn"],
  MUTATION_PROPOSED:  ["Ders çıkarılıyor", "Başarısızlık analiz edildi", "warn"],
  MUTATION_ACCEPTED:  ["Yeni yaklaşım kabul edildi", "Prompt iyileştirildi", "good"],
  MUTATION_REJECTED:  ["Yeni yaklaşım reddedildi", "Eski (daha iyi) sürüm korundu", "warn"],
  CANDIDATE_CREATED:  ["Yeni çözüm denendi", null, "warn"],
  GENERATION_COMPLETED:["Jenerasyon tamamlandı", null, "warn"],
  RUN_PAUSED:         ["Duraklatıldı", null, "warn"],
  RUN_RESUMED:        ["Devam ediliyor", null, "warn"],
  RUN_CANCELLING:     ["İptal ediliyor", null, "warn"],
  RUN_COMPLETED:      ["✅ BAŞARILI — tüm testler geçti", "Ajan problemi çözdü", "good"],
  RUN_FAILED:         ["Başarısız", null, "bad"],
  RUN_CANCELLED:      ["İptal edildi", null, "warn"],
  LIMIT_REACHED:      ["Limit doldu", "Süre/çağrı bütçesi tükendi", "warn"],
};

function detailFor(ev) {
  const p = ev.payload || {};
  switch (ev.kind) {
    case "PROBLEM_LOADED": return p.title ? `${p.title} (${p.problem_id})` : null;
    case "LLM_REQUEST_STARTED": return p.model ? `model: ${p.model}` : null;
    case "LLM_RESPONSE_RECEIVED": return p.bytes != null ? `${p.bytes} bayt yanıt` : null;
    case "GUARDIAN_BLOCKED": return p.rule ? `kural: ${p.rule}` : (p.reason || null);
    case "TESTS_COMPLETED": return p.total != null ? `${p.passed}/${p.total} test geçti` : null;
    case "EVALUATION_COMPLETED": return p.total != null ? `skor ${p.score} — ${p.passed ?? p.tests_passed}/${p.total ?? p.tests_total} test` : null;
    case "CANDIDATE_CREATED": return p.tests_total != null ? `skor ${p.score} (${p.tests_passed}/${p.tests_total})` : null;
    case "GENERATION_COMPLETED": return p.best_score != null ? `en iyi skor: ${p.best_score}` : null;
    case "MUTATION_ACCEPTED": return p.score != null ? `yeni skor: ${p.score}` : null;
    case "SANDBOX_STARTED": return p.sandbox ? `sandbox: ${p.sandbox}` : null;
    default: return null;
  }
}

function fmtTime(ts) {
  try { return new Date(ts).toLocaleTimeString("tr-TR"); } catch { return ""; }
}

function addTimeline(ev) {
  const [title, fixed, cls] = KIND[ev.kind] || [ev.kind, null, ""];
  const detail = fixed || detailFor(ev) || "";
  const li = document.createElement("li");
  const dot = document.createElement("span");
  dot.className = "dot " + (cls || "");
  const mid = document.createElement("div");
  const k = document.createElement("div");
  k.className = "tl-kind";
  k.textContent = title + (ev.generation != null ? ` · jenerasyon ${ev.generation}` : "");
  const d = document.createElement("div");
  d.className = "tl-detail";
  d.textContent = detail;
  mid.appendChild(k);
  if (detail) mid.appendChild(d);
  const t = document.createElement("span");
  t.className = "tl-time";
  t.textContent = fmtTime(ev.timestamp);
  li.append(dot, mid, t);
  const tl = $("timeline");
  tl.appendChild(li);
  tl.scrollTop = tl.scrollHeight;
}

// ---- Sistem durumu ------------------------------------------------------
async function loadStatus() {
  try {
    const s = await api.get("/api/status");
    const ollama = $("pillOllama"), docker = $("pillDocker");
    ollama.className = "pill " + (s.ollama_reachable ? "ok" : "no");
    ollama.innerHTML = `Ollama: <b>${s.ollama_reachable ? "bağlı" : "kapalı"}</b>`;
    docker.className = "pill " + (s.docker_available ? "ok" : "no");
    docker.innerHTML = `Docker: <b>${s.docker_available ? "hazır" : "yok"}</b>`;
    $("pillSandbox").innerHTML = `Sandbox: <b>${s.sandbox_default}</b>`;
    $("pillVersion").textContent = "sürüm " + s.version;
    $("sysInfo").innerHTML = `
      <div><span class="k">Sürüm</span><span class="v">${s.version}</span></div>
      <div><span class="k">Ollama adresi</span><span class="v mono">${s.ollama_host}</span></div>
      <div><span class="k">Kurulu modeller</span><span class="v mono">${(s.ollama_models || []).join(", ") || "—"}</span></div>
      <div><span class="k">Veritabanı</span><span class="v mono">${s.storage_path}</span></div>`;
    if (!s.ollama_reachable) {
      showAlert("Ollama'ya ulaşılamıyor. Gerçek çalışma için Ollama'yı başlatın ve bir model indirin " +
        "(örn. <span class='mono'>ollama pull qwen2.5-coder:1.5b</span>). Şimdilik “Hızlı demo” ile LLM'siz deneyebilirsiniz.");
    } else {
      hideAlert();
    }
    if (!s.ollama_models || s.ollama_models.length === 0) {
      $("modelInput").placeholder = "model indirilmemiş — ollama pull qwen2.5-coder:1.5b";
    } else if (!$("modelInput").value) {
      $("modelInput").value = s.ollama_models[0];
    }
    if (!s.docker_available) $("sandboxSelect").value = "local";
    $("sandboxSelect").title = s.docker_available
      ? "docker: tam izolasyon (önerilen)"
      : "Docker yok — yerel mod güvenli değildir";
  } catch (e) {
    showAlert("Sistem durumu okunamadı: " + e.message);
  }
}

function showAlert(html) { $("alertText").innerHTML = html; $("alertBar").hidden = false; }
function hideAlert() { $("alertBar").hidden = true; }

async function loadProblems() {
  const list = await api.get("/api/problems");
  const sel = $("problemSelect");
  sel.innerHTML = "";
  for (const p of list) {
    const o = document.createElement("option");
    o.value = p.id;
    o.textContent = `${p.title} — ${p.id}`;
    o.title = p.description;
    sel.appendChild(o);
  }
}

// ---- Çalıştırma ---------------------------------------------------------
async function startRun(simulate) {
  hideAlert();
  const body = {
    problem_id: $("problemSelect").value,
    mode: $("modeSelect").value,
    sandbox: $("sandboxSelect").value,
    simulate: !!simulate,
  };
  const model = $("modelInput").value.trim();
  if (model) body.model = model;

  $("startBtn").disabled = true;
  $("demoBtn").disabled = true;
  try {
    const res = await api.post("/api/runs", body);
    openRun(res.run_id);
  } catch (e) {
    showAlert("Çalıştırma başlatılamadı: " + e.message);
  } finally {
    $("startBtn").disabled = false;
    $("demoBtn").disabled = false;
  }
}

function openRun(id) {
  currentRun = id;
  $("runCard").hidden = false;
  $("timelineCard").hidden = false;
  $("candCard").hidden = false;
  $("runId").textContent = id.slice(0, 8);
  $("runStatus").textContent = "başlıyor…";
  $("runGen").textContent = "0";
  $("runScore").textContent = "0";
  $("runLlm").textContent = "0";
  $("scoreBar").style.width = "0%";
  $("timeline").innerHTML = "";
  document.querySelector("#candTable tbody").innerHTML = "";
  $("runCard").scrollIntoView({ behavior: "smooth", block: "start" });
  subscribe(id);
  startPolling(id);
}

function subscribe(id) {
  if (es) { es.close(); es = null; }
  es = new EventSource(`/api/runs/${id}/stream`);
  const known = Object.keys(KIND);
  known.forEach((k) => es.addEventListener(k, (m) => {
    try { addTimeline(JSON.parse(m.data)); } catch (_) {}
  }));
  es.addEventListener("RUN_COMPLETED", () => refreshCandidates(id));
  es.addEventListener("LIMIT_REACHED", () => refreshCandidates(id));
  es.addEventListener("RUN_FAILED", () => refreshCandidates(id));
  es.addEventListener("RUN_CANCELLED", () => refreshCandidates(id));
  es.onerror = () => { /* tarayıcı otomatik yeniden bağlanır */ };
}

function startPolling(id) {
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = setInterval(() => refreshRun(id), 700);
}

const STATUS_TR = {
  queued: "sırada", running: "çalışıyor", paused: "duraklatıldı", cancelling: "iptal ediliyor",
  cancelled: "iptal edildi", completed: "tamamlandı ✅", failed: "başarısız",
  interrupted: "kesintiye uğradı", limit_reached: "limit doldu",
};

async function refreshRun(id) {
  try {
    const r = await api.get(`/api/runs/${id}`);
    $("runStatus").textContent = STATUS_TR[r.status] || r.status;
    $("runGen").textContent = r.generation;
    $("runScore").textContent = r.best_score;
    $("runLlm").textContent = r.total_llm_calls;
    $("scoreBar").style.width = Math.min(100, r.best_score) + "%";
    const terminal = ["completed", "failed", "cancelled", "interrupted", "limit_reached"].includes(r.status);
    if (terminal) {
      clearInterval(pollTimer); pollTimer = null;
      refreshCandidates(id);
      if (r.status === "completed" && r.production_success) {
        hideAlert();
      } else if (r.status === "failed" || r.status === "limit_reached") {
        showAlert("Çalışma başarıyla tamamlanmadı. Zaman çizelgesindeki adımları inceleyin; " +
          "modelin ürettiği kod testleri geçemedi veya bütçe tükendi.");
      }
    }
  } catch (_) { /* geçici hataları yut */ }
}

async function refreshCandidates(id) {
  try {
    const list = await api.get(`/api/runs/${id}/candidates`);
    const tb = document.querySelector("#candTable tbody");
    tb.innerHTML = "";
    for (const c of list) {
      const tr = document.createElement("tr");
      const delta = (c.delta > 0 ? "+" : "") + Number(c.delta).toFixed(1);
      tr.innerHTML = `
        <td>${c.generation}</td>
        <td><b>${c.score}</b></td>
        <td>${c.tests_passed}/${c.tests_total}</td>
        <td>${delta}</td>
        <td><span class="tag ${c.accepted ? "good" : "bad"}">${c.accepted ? "geçti" : "kaldı"}</span></td>`;
      tb.appendChild(tr);
    }
  } catch (_) {}
}

// ---- Kumanda ------------------------------------------------------------
async function control(action) {
  if (!currentRun) return;
  try {
    await api.post(`/api/runs/${currentRun}/${action}`);
  } catch (e) {
    showAlert(`Komut uygulanamadı (${action}): ` + e.message);
  }
}

// ---- Başlat -------------------------------------------------------------
(async function init() {
  $("startBtn").addEventListener("click", () => startRun(false));
  $("demoBtn").addEventListener("click", () => startRun(true));
  $("pauseBtn").addEventListener("click", () => control("pause"));
  $("resumeBtn").addEventListener("click", () => control("resume"));
  $("cancelBtn").addEventListener("click", () => control("cancel"));
  await loadStatus();
  try { await loadProblems(); } catch (e) { showAlert("Problem listesi yüklenemedi: " + e.message); }
  setInterval(loadStatus, 10000);
})();
